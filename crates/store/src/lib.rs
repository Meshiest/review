//! The local cache: pull requests, threads, checks, drafts, viewed files,
//! review marks and subscriptions.
//!
//! Every list and thread paints from here first and is patched when the
//! network answers, so nothing on the UI thread may wait on it. [`Store`]
//! owns one writer thread and a pool of read threads; each method returns a
//! [`Pending`] at once. See `docs/design.md`, "Responsiveness rules".

mod clock;
mod db;
mod draft;
mod error;
mod inbox;
mod keys;
mod maintenance;
mod marks;
mod meta;
mod migrations;
mod pending;
mod snapshot;
mod store;
mod subscription;
mod viewed;

pub use clock::{Clock, ManualClock, SystemClock};
pub use draft::{Draft, DraftAnchor, NewQueued, QueuedComment};
pub use error::{Error, Result};
pub use inbox::CachedInbox;
pub use keys::PrKey;
pub use maintenance::MaintenanceReport;
pub use marks::{ReviewMark, Reviewed, Seen, ViewedProgress};
pub use meta::{FetchMeta, StoredFetchMeta};
pub use pending::Pending;
pub use snapshot::CachedSnapshot;
pub use store::{Options, Store};
pub use subscription::{Subscription, SubscriptionTarget};
pub use viewed::ViewedSet;
