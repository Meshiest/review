//! The shared pull request model, the `Forge` trait, and one backend per forge.
//!
//! - [`model`]: forge-neutral types the UI and the store use.
//! - [`Forge`]: the read operations each backend implements, and
//!   [`Capabilities`], what each forge supports.
//! - [`ForgeHub`]: the forge runtime thread; the UI's only entry point.
//! - [`http`]: the transport seam, rate limiting, ETags and retry.
//! - [`record`]: recording and replaying HTTP for offline tests.
//! - [`auth`]: tokens from `gh`, `glab` and `fj`.
//! - [`watch`]: subscriptions to pull requests and CI runs, polled.

pub mod auth;
pub mod avatar;
mod capabilities;
pub mod ci_log;
mod error;
mod forge;
pub mod forgejo;
pub mod github;
pub mod gitlab;
pub mod http;
mod hub;
pub mod model;
mod people;
pub mod record;
pub mod sandbox;
mod stack;
pub mod watch;

pub use avatar::Avatar;
pub use capabilities::{
    Capabilities, ForgejoVersion, ParseVersionError, ReplyMode, ResolveMode, StackSupport,
    Suggestions, ViewedSync,
};
pub use error::{Error, Result};
pub use forge::{Forge, INBOX_LIMIT};
pub use hub::{ForgeConfig, ForgeHub, Pending, Request, Response, WatchHandle};
pub use people::COLLABORATOR_LIMIT;
pub use stack::stack_by_base_branch;
