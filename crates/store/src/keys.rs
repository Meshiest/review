use forge::model::{PrNumber, PrState, PullRequest, RepoRef};
use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::error::Result;

/// A pull request on one forge: host, repository and number.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrKey {
    pub repo: RepoRef,
    pub number: PrNumber,
}

impl PrKey {
    #[must_use]
    pub fn new(repo: RepoRef, number: PrNumber) -> Self {
        Self { repo, number }
    }

    #[must_use]
    pub fn of(pr: &PullRequest) -> Self {
        Self::new(pr.repo.clone(), pr.number)
    }

    pub(crate) fn cols(&self) -> Cols<'_> {
        Cols {
            host: &self.repo.host,
            repo: self.repo.path(),
            number: number_col(self.number),
        }
    }
}

impl From<&PullRequest> for PrKey {
    fn from(pr: &PullRequest) -> Self {
        Self::of(pr)
    }
}

/// The three key columns every per-pull-request table starts with.
pub(crate) struct Cols<'a> {
    pub host: &'a str,
    pub repo: String,
    pub number: i64,
}

pub(crate) fn number_col(number: PrNumber) -> i64 {
    i64::try_from(number.0).unwrap_or(i64::MAX)
}

pub(crate) fn state_col(state: PrState) -> &'static str {
    match state {
        PrState::Draft => "draft",
        PrState::Open => "open",
        PrState::Merged => "merged",
        PrState::Closed => "closed",
    }
}

/// Starts a write transaction up front, so a writer never fails halfway
/// with `SQLITE_BUSY` while upgrading a read lock.
pub(crate) fn begin(conn: &mut Connection) -> Result<Transaction<'_>> {
    Ok(conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

/// A JSON text column deserialized in place, without copying the text.
pub(crate) fn json_col<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<T> {
    let text = row
        .get_ref(index)?
        .as_str()
        .map_err(|err| crate::Error::Database(err.to_string()))?;
    Ok(serde_json::from_str(text)?)
}
