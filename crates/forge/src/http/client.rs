use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{
    CacheMode, Clock, EtagCache, HttpRequest, HttpResponse, Method, RateLimit, RateLimiter,
    RetryMode, TokioClock, Transport, Url,
};
use crate::auth::{CredentialSource, Token};
use crate::error::{Error, Result};
use crate::model::ForgeKind;

/// Longest block taken from a server's `Retry-After` or quota reset, so a
/// bogus header cannot stall a forge indefinitely or overflow the clock.
const MAX_SERVER_WAIT: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt, for 5xx, network errors and short
    /// rate-limit waits.
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// A rate limit that clears sooner than this is waited out and retried;
    /// a longer one is returned as [`Error::RateLimited`] for the UI.
    pub max_rate_limit_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            max_rate_limit_wait: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    fn backoff(&self, retry: u32) -> Duration {
        self.base_delay
            .saturating_mul(1 << retry.min(16))
            .min(self.max_delay)
    }
}

/// Knobs for [`HttpClient::new`]; [`HttpOptions::for_kind`] is what the app uses.
pub struct HttpOptions {
    pub rate_limit: RateLimit,
    pub retry: RetryPolicy,
    pub etag_capacity: usize,
    pub clock: Arc<dyn Clock>,
}

impl HttpOptions {
    #[must_use]
    pub fn for_kind(kind: ForgeKind) -> Self {
        Self {
            rate_limit: match kind {
                ForgeKind::GitHub => RateLimit::GITHUB,
                ForgeKind::GitLab => RateLimit::GITLAB,
                ForgeKind::Forgejo => RateLimit::FORGEJO,
            },
            retry: RetryPolicy::default(),
            etag_capacity: 512,
            clock: Arc::new(TokioClock::new()),
        }
    }
}

/// One per backend. Adds the credential, waits for a rate-limit permit,
/// sends `If-None-Match` for cached GETs, retries what may be retried, and
/// turns failures into [`Error`]s. Cheap to clone.
#[derive(Clone)]
pub struct HttpClient {
    inner: Arc<Inner>,
}

struct Inner {
    kind: ForgeKind,
    host: String,
    transport: Arc<dyn Transport>,
    credentials: Arc<dyn CredentialSource>,
    authorization: tokio::sync::Mutex<Option<Token>>,
    limiter: Mutex<RateLimiter>,
    etags: Mutex<EtagCache>,
    retry: RetryPolicy,
    clock: Arc<dyn Clock>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl HttpClient {
    #[must_use]
    pub fn new(
        kind: ForgeKind,
        host: impl Into<String>,
        transport: Arc<dyn Transport>,
        credentials: Arc<dyn CredentialSource>,
        options: HttpOptions,
    ) -> Self {
        let now = options.clock.now();
        Self {
            inner: Arc::new(Inner {
                kind,
                host: host.into(),
                transport,
                credentials,
                authorization: tokio::sync::Mutex::new(None),
                limiter: Mutex::new(RateLimiter::new(options.rate_limit, now)),
                etags: Mutex::new(EtagCache::new(options.etag_capacity)),
                retry: options.retry,
                clock: options.clock,
            }),
        }
    }

    #[must_use]
    pub fn kind(&self) -> ForgeKind {
        self.inner.kind
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.inner.host
    }

    /// Time on this client's clock, for caches that expire.
    #[must_use]
    pub fn now(&self) -> std::time::Duration {
        self.inner.clock.now()
    }

    /// Loads the credential now instead of on the first request; the hub
    /// calls it at startup.
    ///
    /// # Errors
    /// [`Error::Auth`] or [`Error::Cli`] when no token can be read.
    pub async fn authorize(&self) -> Result<()> {
        self.authorization(false).await.map(drop)
    }

    async fn authorization(&self, refresh: bool) -> Result<Token> {
        let mut slot = self.inner.authorization.lock().await;
        if !refresh && let Some(token) = slot.as_ref() {
            return Ok(token.clone());
        }
        let credential = if refresh {
            self.inner.credentials.refresh().await?
        } else {
            self.inner.credentials.load().await?
        };
        let token = credential.authorization();
        *slot = Some(token.clone());
        Ok(token)
    }

    /// Sends with the client's policy. A success, or a `304` answered from
    /// the ETag cache, is `Ok`; every other status is an `Err`.
    ///
    /// # Errors
    /// - [`Error::NotFound`] on 404.
    /// - [`Error::Auth`] on 401 after one credential refresh.
    /// - [`Error::RateLimited`] when the limit clears later than
    ///   [`RetryPolicy::max_rate_limit_wait`] or retries ran out.
    /// - [`Error::Network`] or [`Error::Status`] after retries.
    pub async fn send(&self, mut request: HttpRequest) -> Result<HttpResponse> {
        if request.authorization.is_none() {
            request.authorization = Some(self.authorization(false).await?);
        }
        let cache_key = (request.method == Method::Get && request.cache == CacheMode::Etag)
            .then(|| request.url.to_string());
        if let Some(key) = &cache_key
            && let Some(etag) = lock(&self.inner.etags).etag(key)
        {
            request
                .headers
                .push(("if-none-match".to_owned(), etag.to_owned()));
        }
        let retry = self.inner.retry;
        let may_retry =
            |retries: u32| request.retry == RetryMode::Safe && retries < retry.max_retries;
        let mut retries = 0;
        let mut refreshed = false;
        loop {
            self.acquire().await;
            let response = match self.inner.transport.send(&request).await {
                Ok(response) => response,
                Err(err @ Error::Network { .. }) if may_retry(retries) => {
                    drop(err);
                    self.inner.clock.sleep(retry.backoff(retries)).await;
                    retries += 1;
                    continue;
                }
                Err(err) => return Err(err),
            };
            self.observe_quota(&response);

            if response.status == 304
                && let Some(key) = &cache_key
                && let Some(cached) = lock(&self.inner.etags).cached(key)
            {
                return Ok(cached);
            }
            if response.is_success() {
                if let Some(key) = cache_key
                    && let Some(etag) = response.header("etag")
                {
                    lock(&self.inner.etags).insert(key, etag.to_owned(), response.clone());
                }
                return Ok(response);
            }
            if request.accept.contains(&response.status) {
                return Ok(response);
            }
            if response.status == 401 {
                if refreshed {
                    return Err(Error::Auth {
                        host: self.inner.host.clone(),
                        reason: "the forge rejected the token (HTTP 401)".to_owned(),
                    });
                }
                refreshed = true;
                request.authorization = Some(self.authorization(true).await?);
                continue;
            }
            if let Some(wait) = self.rate_limit_wait(&response) {
                let wait = wait.min(MAX_SERVER_WAIT);
                let now = self.inner.clock.now();
                lock(&self.inner.limiter).block_until(now.saturating_add(wait));
                if wait <= retry.max_rate_limit_wait && may_retry(retries) {
                    retries += 1;
                    continue;
                }
                return Err(Error::RateLimited {
                    host: self.inner.host.clone(),
                    retry_after: Some(wait),
                });
            }
            if matches!(response.status, 500 | 502 | 503 | 504) && may_retry(retries) {
                let delay = retry
                    .backoff(retries)
                    .max(response.retry_after().unwrap_or_default())
                    .min(retry.max_delay);
                self.inner.clock.sleep(delay).await;
                retries += 1;
                continue;
            }
            return Err(status_error(&request, &response));
        }
    }

    /// GET and decode JSON.
    ///
    /// # Errors
    /// As [`Self::send`], plus [`Error::Decode`].
    pub async fn get_json<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        let request = HttpRequest::get(url);
        let describe = request.describe();
        self.send(request).await?.json(&describe)
    }

    /// A GraphQL query (retried like a GET). The body is
    /// `{operationName, query, variables}`; recordings are matched on the
    /// operation name and variables, so name every operation.
    ///
    /// # Errors
    /// As [`Self::send`]; [`Error::GraphQl`] when the response has
    /// `errors`, [`Error::NotFound`] for a `NOT_FOUND` error and
    /// [`Error::RateLimited`] for `RATE_LIMITED`.
    pub async fn graphql<T: DeserializeOwned>(
        &self,
        url: Url,
        operation: &str,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        self.graphql_with(url, operation, query, variables, RetryMode::Safe)
            .await
    }

    /// A GraphQL mutation: like [`Self::graphql`], but never resent after a
    /// 5xx or a lost response, which may have applied it.
    ///
    /// # Errors
    /// As [`Self::graphql`].
    pub async fn graphql_mutation<T: DeserializeOwned>(
        &self,
        url: Url,
        operation: &str,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        self.graphql_with(url, operation, query, variables, RetryMode::Never)
            .await
    }

    async fn graphql_with<T: DeserializeOwned>(
        &self,
        url: Url,
        operation: &str,
        query: &str,
        variables: serde_json::Value,
        retry: RetryMode,
    ) -> Result<T> {
        let body = serde_json::json!({
            "operationName": operation,
            "query": query,
            "variables": variables,
        });
        let request = HttpRequest::post_json(url, body).retry(retry);
        let describe = format!("{} {operation}", request.describe());
        let envelope: GraphQlEnvelope<T> = self.send(request).await?.json(&describe)?;
        if envelope.errors.is_empty()
            && let Some(data) = envelope.data
        {
            return Ok(data);
        }
        let kind_is = |kind: &str| {
            envelope
                .errors
                .iter()
                .any(|e| e.kind.as_deref() == Some(kind))
        };
        if kind_is("RATE_LIMITED") {
            return Err(Error::RateLimited {
                host: self.inner.host.clone(),
                retry_after: None,
            });
        }
        if kind_is("NOT_FOUND") {
            return Err(Error::NotFound { request: describe });
        }
        let message = if envelope.errors.is_empty() {
            "no data".to_owned()
        } else {
            envelope
                .errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        };
        Err(Error::GraphQl {
            operation: operation.to_owned(),
            message,
        })
    }

    async fn acquire(&self) {
        loop {
            let now = self.inner.clock.now();
            let wait = lock(&self.inner.limiter).try_acquire(now);
            match wait {
                Ok(()) => return,
                Err(wait) => self.inner.clock.sleep(wait).await,
            }
        }
    }

    /// GitHub `x-ratelimit-*`, GitLab `ratelimit-*`; Forgejo sends neither.
    fn quota(&self, response: &HttpResponse) -> Option<(u64, Duration)> {
        let header = |name: &str| {
            response
                .header(&format!("x-{name}"))
                .or_else(|| response.header(name))
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let remaining = header("ratelimit-remaining")?;
        let reset = Duration::from_secs(header("ratelimit-reset")?);
        Some((
            remaining,
            reset
                .saturating_sub(self.inner.clock.unix_time())
                .min(MAX_SERVER_WAIT),
        ))
    }

    fn observe_quota(&self, response: &HttpResponse) {
        if let Some((remaining, reset_in)) = self.quota(response) {
            let now = self.inner.clock.now();
            lock(&self.inner.limiter).observe_quota(remaining, reset_in, now);
        }
    }

    /// How long to wait, when the response is a primary or secondary rate
    /// limit: 429, or a GitHub 403 with `Retry-After`, an empty quota, or a
    /// "secondary rate limit" message.
    fn rate_limit_wait(&self, response: &HttpResponse) -> Option<Duration> {
        const DEFAULT_WAIT: Duration = Duration::from_secs(60);
        let quota = self.quota(response);
        let exhausted = quota
            .filter(|(remaining, _)| *remaining == 0)
            .map(|(_, reset)| reset);
        let limited = match response.status {
            429 => true,
            403 => {
                response.retry_after().is_some()
                    || exhausted.is_some()
                    || response.text().to_ascii_lowercase().contains("rate limit")
            }
            _ => false,
        };
        limited.then(|| response.retry_after().or(exhausted).unwrap_or(DEFAULT_WAIT))
    }
}

#[derive(Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
    #[serde(rename = "type")]
    kind: Option<String>,
}

fn status_error(request: &HttpRequest, response: &HttpResponse) -> Error {
    let describe = request.describe();
    if response.status == 404 {
        return Error::NotFound { request: describe };
    }
    // Forges put the reason in `message` (GitHub, GitLab, Forgejo) or `error`.
    let message = serde_json::from_slice::<serde_json::Value>(&response.body)
        .ok()
        .and_then(|body| {
            ["message", "error"].iter().find_map(|key| {
                body.get(key)
                    .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            })
        })
        .unwrap_or_else(|| response.text());
    Error::Status {
        request: describe,
        status: response.status,
        message: message.chars().take(300).collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU32, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::auth::{Credential, StaticCredential};
    use crate::http::ManualClock;

    /// Serves canned responses in order and records what it was sent.
    #[derive(Default)]
    struct Script {
        responses: Mutex<VecDeque<Result<HttpResponse>>>,
        sent: Mutex<Vec<HttpRequest>>,
    }

    #[async_trait]
    impl Transport for Script {
        async fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
            lock(&self.sent).push(request.clone());
            lock(&self.responses)
                .pop_front()
                .expect("script ran out of responses")
        }
    }

    fn reply(status: u16, headers: &[(&str, &str)], body: &str) -> Result<HttpResponse> {
        Ok(HttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.as_bytes().to_vec(),
            cached: false,
        })
    }

    const UNIX_START: Duration = Duration::from_secs(1_700_000_000);

    fn client(
        responses: Vec<Result<HttpResponse>>,
        credentials: Arc<dyn CredentialSource>,
    ) -> (HttpClient, Arc<Script>, Arc<ManualClock>) {
        let script = Arc::new(Script {
            responses: Mutex::new(responses.into()),
            sent: Mutex::default(),
        });
        let clock = Arc::new(ManualClock::new(UNIX_START));
        let options = HttpOptions {
            rate_limit: RateLimit {
                burst: 100,
                per_second: 100.0,
            },
            retry: RetryPolicy::default(),
            etag_capacity: 8,
            clock: clock.clone(),
        };
        let http = HttpClient::new(
            ForgeKind::GitHub,
            "github.com",
            script.clone(),
            credentials,
            options,
        );
        (http, script, clock)
    }

    fn static_token() -> Arc<dyn CredentialSource> {
        Arc::new(StaticCredential(Credential::bearer(Token::new("t0"))))
    }

    fn url(path: &str) -> Url {
        Url::parse("https://api.github.com/")
            .unwrap()
            .join(path)
            .unwrap()
    }

    #[tokio::test]
    async fn retries_server_errors_with_backoff() {
        let (http, script, clock) = client(
            vec![
                reply(502, &[], ""),
                reply(503, &[], ""),
                reply(200, &[], "{}"),
            ],
            static_token(),
        );
        http.send(HttpRequest::get(url("user"))).await.unwrap();
        assert_eq!(
            clock.sleeps(),
            [Duration::from_millis(500), Duration::from_secs(1)]
        );
        assert_eq!(lock(&script.sent).len(), 3);
    }

    #[tokio::test]
    async fn writes_are_not_retried() {
        let (http, _, clock) = client(
            vec![reply(503, &[], "{\"message\":\"down\"}")],
            static_token(),
        );
        let request = HttpRequest::post_json(url("x"), serde_json::json!({}));
        let err = http.send(request).await.unwrap_err();
        assert_eq!(
            err,
            Error::Status {
                request: "POST /x".to_owned(),
                status: 503,
                message: "down".to_owned()
            }
        );
        assert!(clock.sleeps().is_empty());
    }

    #[tokio::test]
    async fn graphql_mutations_are_not_retried_but_queries_are() {
        let (http, script, clock) = client(
            vec![
                reply(502, &[], ""),
                reply(502, &[], ""),
                reply(200, &[], r#"{"data":{"ok":true}}"#),
            ],
            static_token(),
        );
        let vars = serde_json::json!({});
        let err = http
            .graphql_mutation::<serde_json::Value>(
                url("graphql"),
                "M",
                "mutation M{x}",
                vars.clone(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Status { status: 502, .. }), "{err:?}");
        assert_eq!(lock(&script.sent).len(), 1);
        assert!(clock.sleeps().is_empty());
        http.graphql::<serde_json::Value>(url("graphql"), "Q", "query Q{x}", vars)
            .await
            .unwrap();
        assert_eq!(lock(&script.sent).len(), 3);
    }

    #[tokio::test]
    async fn waits_out_a_secondary_rate_limit() {
        let (http, _, clock) = client(
            vec![
                reply(
                    403,
                    &[("retry-after", "30")],
                    "{\"message\":\"secondary rate limit\"}",
                ),
                reply(200, &[], "{}"),
            ],
            static_token(),
        );
        http.send(HttpRequest::get(url("user"))).await.unwrap();
        assert_eq!(clock.sleeps(), [Duration::from_secs(30)]);
    }

    #[tokio::test]
    async fn long_rate_limits_go_to_the_caller() {
        let reset = (UNIX_START.as_secs() + 3600).to_string();
        let (http, _, _) = client(
            vec![reply(
                403,
                &[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", &reset),
                ],
                "{}",
            )],
            static_token(),
        );
        let err = http.send(HttpRequest::get(url("user"))).await.unwrap_err();
        assert_eq!(
            err,
            Error::RateLimited {
                host: "github.com".to_owned(),
                retry_after: Some(Duration::from_secs(3600))
            }
        );
    }

    #[tokio::test]
    async fn an_empty_quota_holds_the_next_request_until_reset() {
        let reset = (UNIX_START.as_secs() + 10).to_string();
        let (http, _, clock) = client(
            vec![
                reply(
                    200,
                    &[
                        ("x-ratelimit-remaining", "0"),
                        ("x-ratelimit-reset", &reset),
                    ],
                    "{}",
                ),
                reply(200, &[], "{}"),
            ],
            static_token(),
        );
        http.send(HttpRequest::get(url("a"))).await.unwrap();
        assert!(clock.sleeps().is_empty());
        http.send(HttpRequest::get(url("b"))).await.unwrap();
        assert_eq!(clock.sleeps(), [Duration::from_secs(10)]);
    }

    #[tokio::test]
    async fn not_modified_is_served_from_the_etag_cache() {
        let (http, script, _) = client(
            vec![
                reply(200, &[("etag", "W/\"v1\"")], "[1]"),
                reply(304, &[], ""),
            ],
            static_token(),
        );
        let first = http.send(HttpRequest::get(url("pulls"))).await.unwrap();
        assert!(!first.cached);
        let second = http.send(HttpRequest::get(url("pulls"))).await.unwrap();
        assert!(second.cached);
        assert_eq!(second.body, b"[1]");
        let sent = lock(&script.sent);
        assert!(sent[0].headers.is_empty());
        assert_eq!(
            sent[1].headers,
            [("if-none-match".to_owned(), "W/\"v1\"".to_owned())]
        );
    }

    #[tokio::test]
    async fn a_bypassing_request_neither_sends_nor_stores_an_etag() {
        let (http, script, _) = client(
            vec![
                reply(200, &[("etag", "\"v1\"")], "log"),
                reply(206, &[("etag", "\"v1\"")], "og"),
                reply(416, &[("content-range", "bytes */3")], ""),
            ],
            static_token(),
        );
        let log = || {
            HttpRequest::get(url("logs"))
                .cache(CacheMode::Bypass)
                .accept(&[416])
        };
        assert_eq!(http.send(log()).await.unwrap().body, b"log");
        let second = http.send(log().header("range", "bytes=1-")).await.unwrap();
        assert!(!second.cached);
        assert_eq!(second.status, 206);
        assert_eq!(http.send(log()).await.unwrap().status, 416);
        assert!(
            lock(&script.sent)
                .iter()
                .all(|r| r.header_value("if-none-match").is_none())
        );
    }

    struct Rotating(AtomicU32);

    #[async_trait]
    impl CredentialSource for Rotating {
        async fn load(&self) -> Result<Credential> {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Credential::bearer(Token::new(format!("t{n}"))))
        }
    }

    #[tokio::test]
    async fn a_401_refreshes_the_credential_once() {
        let (http, script, _) = client(
            vec![reply(401, &[], ""), reply(200, &[], "{}")],
            Arc::new(Rotating(AtomicU32::new(0))),
        );
        http.send(HttpRequest::get(url("user"))).await.unwrap();
        let sent = lock(&script.sent);
        let auth: Vec<_> = sent
            .iter()
            .map(|r| r.authorization.as_ref().unwrap().expose().to_owned())
            .collect();
        assert_eq!(auth, ["Bearer t0", "Bearer t1"]);
    }

    #[tokio::test]
    async fn a_second_401_is_an_auth_error() {
        let (http, _, _) = client(
            vec![reply(401, &[], ""), reply(401, &[], "")],
            static_token(),
        );
        let err = http.send(HttpRequest::get(url("user"))).await.unwrap_err();
        assert!(matches!(err, Error::Auth { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn graphql_errors_are_typed() {
        let (http, _, _) = client(
            vec![
                reply(200, &[], r#"{"data":{"viewer":{"login":"me"}}}"#),
                reply(
                    200,
                    &[],
                    r#"{"data":null,"errors":[{"type":"NOT_FOUND","message":"nope"}]}"#,
                ),
                reply(200, &[], r#"{"errors":[{"message":"bad field"}]}"#),
            ],
            static_token(),
        );
        let gql = url("graphql");
        let vars = serde_json::json!({});
        let data: serde_json::Value = http
            .graphql(
                gql.clone(),
                "Viewer",
                "query Viewer { viewer { login } }",
                vars.clone(),
            )
            .await
            .unwrap();
        assert_eq!(data["viewer"]["login"], "me");
        let err = http
            .graphql::<serde_json::Value>(gql.clone(), "Pr", "q", vars.clone())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }), "{err:?}");
        let err = http
            .graphql::<serde_json::Value>(gql, "Bad", "q", vars)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::GraphQl {
                operation: "Bad".to_owned(),
                message: "bad field".to_owned()
            }
        );
    }
}
