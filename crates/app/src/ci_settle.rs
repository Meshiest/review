//! Rows whose CI shows queued or running are read again until it settles,
//! watched or not. Nothing else is sure to do it: a watch's first poll only
//! records a baseline, an inbox refresh can fail and keep the old list, and
//! a pull request nobody watches is only read with the inbox.
//!
//! [`crate::session::Session`] runs this every [`SETTLE_POLL`] while any row
//! is active and stops once none is; a settled row is never read here.

use std::time::Duration;

use forge::model::{CiState, PrNumber, RepoRef};

use crate::model::PrKey;

/// How often rows with active CI are read again.
pub const SETTLE_POLL: Duration = Duration::from_secs(60);

/// One batched read: the pull requests of one repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettleRead {
    pub repo: RepoRef,
    pub numbers: Vec<PrNumber>,
}

/// The reads for every pull request shown with active CI, one per
/// repository in the order first seen, each pull request once.
#[must_use]
pub fn reads<'a>(shown: impl IntoIterator<Item = (&'a PrKey, CiState)>) -> Vec<SettleRead> {
    let mut reads: Vec<SettleRead> = Vec::new();
    for (key, ci) in shown {
        if !ci.is_active() {
            continue;
        }
        match reads.iter_mut().find(|read| read.repo == key.repo) {
            Some(read) if read.numbers.contains(&key.number) => {}
            Some(read) => read.numbers.push(key.number),
            None => reads.push(SettleRead {
                repo: key.repo.clone(),
                numbers: vec![key.number],
            }),
        }
    }
    reads
}

/// The pull requests of `read` with the state the forge answered.
pub fn answered<'a>(
    read: &'a SettleRead,
    states: &'a [CiState],
) -> impl Iterator<Item = (PrKey, CiState)> + 'a {
    read.numbers
        .iter()
        .zip(states)
        .map(|(&number, &state)| (PrKey::new(read.repo.clone(), number), state))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(repo: &str, n: u64) -> PrKey {
        PrKey::new(RepoRef::new("gitlab.com", "rackn", repo), PrNumber(n))
    }

    #[test]
    fn only_active_rows_are_read_one_batch_per_repository() {
        let (a29, a30, a31, b7) = (
            key("rackn.com", 29),
            key("rackn.com", 30),
            key("rackn.com", 31),
            key("other", 7),
        );
        let shown = [
            (&a29, CiState::Running),
            (&b7, CiState::Queued),
            (&a30, CiState::Pass),
            (&a31, CiState::Queued),
            (&a29, CiState::Running),
        ];
        let reads = reads(shown);
        assert_eq!(
            reads,
            [
                SettleRead {
                    repo: a29.repo.clone(),
                    numbers: vec![PrNumber(29), PrNumber(31)],
                },
                SettleRead {
                    repo: b7.repo.clone(),
                    numbers: vec![PrNumber(7)],
                },
            ]
        );
    }

    #[test]
    fn settled_rows_are_not_read() {
        let keys: Vec<PrKey> = (1..=6).map(|n| key("r", n)).collect();
        let states = [
            CiState::Pass,
            CiState::Fail,
            CiState::Canceled,
            CiState::Skipped,
            CiState::None,
            CiState::Pass,
        ];
        assert!(reads(keys.iter().zip(states)).is_empty());
    }

    #[test]
    fn answers_pair_with_the_numbers_asked() {
        let read = SettleRead {
            repo: RepoRef::new("gitlab.com", "rackn", "rackn.com"),
            numbers: vec![PrNumber(29), PrNumber(31)],
        };
        let got: Vec<(PrKey, CiState)> = answered(&read, &[CiState::Pass, CiState::None]).collect();
        assert_eq!(
            got,
            [
                (key("rackn.com", 29), CiState::Pass),
                (key("rackn.com", 31), CiState::None),
            ]
        );
    }
}
