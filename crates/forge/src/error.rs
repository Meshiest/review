use std::time::Duration;

use crate::model::ForgeKind;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a redirect was not followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RedirectRefusal {
    #[error("more than {} redirects", crate::http::MAX_REDIRECTS)]
    TooMany,
    #[error("the Location is not a usable http(s) URL")]
    BadLocation,
    #[error("it goes from https to http")]
    Downgrade,
    #[error("it leaves the request's origin")]
    CrossOrigin,
    #[error("the target is not on the allowlist")]
    NotAllowed,
}

/// Errors carry text, not source errors, so they are `Clone` and can be sent
/// to the UI and shared between waiters. None of them ever contains a token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// No usable credential, or the forge rejected it after a refresh.
    #[error("not signed in to {host}: {reason}")]
    Auth { host: String, reason: String },

    /// A CLI that holds the token failed or is missing.
    #[error("`{program}` failed: {reason}")]
    Cli { program: String, reason: String },

    #[error("not found: {request}")]
    NotFound { request: String },

    /// The forge's limit is exhausted for longer than a retry waits.
    #[error("rate limited by {host}; retry after {retry_after:?}")]
    RateLimited {
        host: String,
        retry_after: Option<Duration>,
    },

    /// Any other non-success status, after retries.
    #[error("{request}: HTTP {status}: {message}")]
    Status {
        request: String,
        status: u16,
        message: String,
    },

    /// Connection, TLS or timeout failure, after retries.
    #[error("network error on {request}: {reason}")]
    Network { request: String, reason: String },

    /// A redirect the request's policy does not allow. The target is left
    /// out: it is server-chosen and may carry a presigned query.
    #[error("{request}: redirect refused: {reason}")]
    Redirect {
        request: String,
        reason: RedirectRefusal,
    },

    /// The response body is over the request's `max_body`; the rest was not read.
    #[error("{request}: response is over the {limit} byte limit")]
    TooLarge { request: String, limit: usize },

    /// The response did not have the expected shape.
    #[error("unexpected response from {request}: {reason}")]
    Decode { request: String, reason: String },

    /// The forge has not finished preparing what the request needs (a
    /// GitLab merge request whose diff is not computed yet); trying again
    /// in a moment may work.
    #[error("{what} is not ready on {host} yet; try again in a few seconds")]
    NotReady { host: String, what: String },

    /// A GraphQL response with `errors`.
    #[error("GraphQL {operation}: {message}")]
    GraphQl { operation: String, message: String },

    /// The forge has no API for this (see [`crate::Capabilities`]).
    #[error("{feature} is not supported on {kind}")]
    Unsupported {
        kind: ForgeKind,
        feature: &'static str,
    },

    /// The forge renders suggestions but cannot apply them ([`crate::Suggestions::Rendered`]
    /// or `NotRendered`): the `repo` crate applies them as a local commit.
    #[error("{kind} cannot apply suggestions; apply them locally")]
    ApplyLocally { kind: ForgeKind },

    /// A replayed test made a request that has no recording.
    #[error("no recording in {cassette} for {request}")]
    ReplayMiss { cassette: String, request: String },

    #[error("recording {path}: {reason}")]
    Recording { path: String, reason: String },

    #[error("no forge configured for host {0}")]
    UnknownForge(String),

    /// A configured host that does not make a valid API URL.
    #[error("bad forge host {host:?}: {reason}")]
    Config { host: String, reason: String },

    /// The forge runtime stopped, or the request was dropped before it
    /// finished.
    #[error("forge runtime stopped")]
    Shutdown,
}
