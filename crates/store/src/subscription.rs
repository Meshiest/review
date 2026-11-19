use forge::model::{RepoRef, RunId, Timestamp};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::clock::from_ms;
use crate::error::Result;
use crate::keys::{PrKey, begin};

/// What is being watched.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionTarget {
    PullRequest { repo: RepoRef, number: u64 },
    Run { repo: RepoRef, run: RunId },
}

impl SubscriptionTarget {
    #[must_use]
    pub fn pull_request(key: &PrKey) -> Self {
        Self::PullRequest {
            repo: key.repo.clone(),
            number: key.number.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscription {
    pub target: SubscriptionTarget,
    pub created_at: Timestamp,
    /// What the poller last saw, opaque to the store (for example a JSON
    /// rollup), so a change can be told from a first look.
    pub state: Option<String>,
}

fn target_col(target: &SubscriptionTarget) -> Result<String> {
    Ok(serde_json::to_string(target)?)
}

/// Adds the subscription, or replaces `state` of an existing one (its
/// `created_at` is kept).
pub(crate) fn put_subscription(
    conn: &mut Connection,
    now: i64,
    target: &SubscriptionTarget,
    state: Option<&str>,
) -> Result<()> {
    let target = target_col(target)?;
    let tx = begin(conn)?;
    tx.execute(
        "INSERT INTO subscriptions (target, created_at, state) VALUES (?1, ?2, ?3)
         ON CONFLICT (target) DO UPDATE SET state = excluded.state",
        params![target, now, state],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn delete_subscription(
    conn: &mut Connection,
    target: &SubscriptionTarget,
) -> Result<()> {
    let target = target_col(target)?;
    let tx = begin(conn)?;
    tx.execute("DELETE FROM subscriptions WHERE target = ?1", [target])?;
    tx.commit()?;
    Ok(())
}

/// All subscriptions, oldest first.
pub(crate) fn read_subscriptions(conn: &Connection) -> Result<Vec<Subscription>> {
    let mut stmt = conn.prepare_cached(
        "SELECT target, created_at, state FROM subscriptions ORDER BY created_at, target",
    )?;
    let mut rows = stmt.query([])?;
    let mut subscriptions = Vec::new();
    while let Some(row) = rows.next()? {
        let target: String = row.get(0)?;
        subscriptions.push(Subscription {
            target: serde_json::from_str(&target)?,
            created_at: from_ms(row.get(1)?),
            state: row.get(2)?,
        });
    }
    Ok(subscriptions)
}
