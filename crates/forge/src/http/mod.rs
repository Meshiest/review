//! HTTP for every backend. All requests go through a [`Transport`], so tests
//! swap the network for recordings ([`crate::record`]). [`HttpClient`] adds
//! the policy on top: auth, per-forge rate limiting, ETag caching and retry.

mod client;
mod clock;
mod etag;
mod rate;
mod redirect;
mod transport;

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
pub use client::{HttpClient, HttpOptions, RetryPolicy};
pub use clock::{Clock, ManualClock, TokioClock};
pub use etag::EtagCache;
pub use rate::{RateLimit, RateLimiter};
pub use redirect::{MAX_REDIRECTS, RedirectPolicy, UrlFilter};
pub use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::auth::Token;
pub use crate::error::RedirectRefusal;
use crate::error::{Error, Result};
pub use transport::ReqwestTransport;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a failed request may be sent again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryMode {
    /// Reads, including GraphQL queries sent as POST.
    Safe,
    /// Writes: a retry after a lost response could apply them twice.
    Never,
}

/// Whether a GET goes through the [`EtagCache`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CacheMode {
    /// Send `If-None-Match` and keep the response for a `304`.
    #[default]
    Etag,
    /// Neither: for large bodies that must not be held twice (job logs),
    /// and for requests whose headers select part of the body (`Range`),
    /// which the URL-keyed cache cannot tell apart.
    Bypass,
}

#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: Method,
    pub url: Url,
    /// Lowercase names. Never the `Authorization` header: that is `authorization`.
    pub headers: Vec<(String, String)>,
    /// A JSON body (REST payload or GraphQL `{operationName, query, variables}`).
    pub body: Option<serde_json::Value>,
    /// The whole header value; [`HttpClient`] fills it from the backend's credential.
    pub authorization: Option<Token>,
    pub retry: RetryMode,
    /// Largest response body taken. The [`ReqwestTransport`] stops reading
    /// at this many (decoded) bytes and fails with [`Error::TooLarge`].
    pub max_body: usize,
    /// Which redirects the transport may follow. The default never leaves
    /// the request's origin.
    pub redirects: RedirectPolicy,
    pub cache: CacheMode,
    /// Statuses outside 2xx that [`HttpClient::send`] returns as a response
    /// instead of an error, because the caller reads them (a `416` for a log
    /// range past the end carries the log's length).
    pub accept: &'static [u16],
}

/// The default [`HttpRequest::max_body`]: generous for API pages and diffs.
pub const DEFAULT_MAX_BODY: usize = 64 * 1024 * 1024;

impl HttpRequest {
    #[must_use]
    pub fn get(url: Url) -> Self {
        Self {
            method: Method::Get,
            url,
            headers: Vec::new(),
            body: None,
            authorization: None,
            retry: RetryMode::Safe,
            max_body: DEFAULT_MAX_BODY,
            redirects: RedirectPolicy::SameOrigin,
            cache: CacheMode::Etag,
            accept: &[],
        }
    }

    /// A JSON POST. Marked [`RetryMode::Never`]; call [`Self::retry`] for a
    /// read such as a GraphQL query.
    #[must_use]
    pub fn post_json(url: Url, body: serde_json::Value) -> Self {
        Self {
            method: Method::Post,
            body: Some(body),
            retry: RetryMode::Never,
            ..Self::get(url)
        }
    }

    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_ascii_lowercase(), value.into()));
        self
    }

    #[must_use]
    pub fn retry(mut self, retry: RetryMode) -> Self {
        self.retry = retry;
        self
    }

    #[must_use]
    pub fn max_body(mut self, max_body: usize) -> Self {
        self.max_body = max_body;
        self
    }

    #[must_use]
    pub fn redirects(mut self, redirects: RedirectPolicy) -> Self {
        self.redirects = redirects;
        self
    }

    #[must_use]
    pub fn cache(mut self, cache: CacheMode) -> Self {
        self.cache = cache;
        self
    }

    #[must_use]
    pub fn accept(mut self, statuses: &'static [u16]) -> Self {
        self.accept = statuses;
        self
    }

    /// The value of a header set on the request (lowercase name).
    #[must_use]
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// `GET /path?query`, for errors and logs; no host, no secrets.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.url.query() {
            Some(query) => format!("{} {}?{query}", self.method, self.url.path()),
            None => format!("{} {}", self.method, self.url.path()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// Lowercase names, in the order received.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Served from the ETag cache after a `304 Not Modified`: nothing changed
    /// since the last fetch of this URL, and `status` is the cached one.
    pub cached: bool,
}

impl HttpResponse {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// # Errors
    /// [`Error::Decode`] when the body is not `T` as JSON.
    pub fn json<T: DeserializeOwned>(&self, request: &str) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(|err| Error::Decode {
            request: request.to_owned(),
            reason: err.to_string(),
        })
    }

    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The `rel="next"` URL of a `Link` header (GitHub REST, GitLab REST,
    /// Forgejo all paginate this way).
    #[must_use]
    pub fn next_page(&self) -> Option<Url> {
        let link = self.header("link")?;
        link.split(',').find_map(|part| {
            let (target, params) = part.split_once(';')?;
            let is_next = params
                .split(';')
                .any(|p| matches!(p.trim(), "rel=\"next\"" | "rel=next"));
            let target = target.trim().strip_prefix('<')?.strip_suffix('>')?;
            if is_next {
                Url::parse(target).ok()
            } else {
                None
            }
        })
    }

    /// `Retry-After` in seconds (the HTTP-date form is not used by forges).
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        self.header("retry-after")?
            .trim()
            .parse()
            .ok()
            .map(Duration::from_secs)
    }
}

/// Sends one request and returns the response as received: no retry, no
/// caching, any status. Errors are only for requests that got no response.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn send(&self, request: &HttpRequest) -> Result<HttpResponse>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(headers: &[(&str, &str)]) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: Vec::new(),
            cached: false,
        }
    }

    #[test]
    fn finds_the_next_page_link() {
        let link = "<https://api.github.com/x?page=1>; rel=\"prev\", <https://api.github.com/x?page=3>; rel=\"next\", <https://api.github.com/x?page=9>; rel=\"last\"";
        let next = response(&[("link", link)]).next_page().unwrap();
        assert_eq!(next.as_str(), "https://api.github.com/x?page=3");
        assert!(response(&[]).next_page().is_none());
    }

    #[test]
    fn describe_has_no_host() {
        let url = Url::parse("https://gitlab.com/api/v4/merge_requests?scope=all").unwrap();
        assert_eq!(
            HttpRequest::get(url).describe(),
            "GET /api/v4/merge_requests?scope=all"
        );
    }
}
