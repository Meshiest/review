use forge::model::{Sha, Snapshot, Timestamp};
use rusqlite::{Connection, params};

use crate::clock::from_ms;
use crate::error::Result;
use crate::inbox::upsert_pr;
use crate::keys::{PrKey, begin, json_col};

/// A pull request with its threads and checks as last fetched.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedSnapshot {
    pub snapshot: Snapshot,
    /// The head commit the snapshot was fetched at. Diff and thread
    /// positions are only valid against this commit.
    pub head_sha: Sha,
    pub fetched_at: Timestamp,
}

/// Stores the snapshot and refreshes the pull request row it contains, in
/// one transaction.
pub(crate) fn put_snapshot(conn: &mut Connection, now: i64, snapshot: &Snapshot) -> Result<()> {
    let pr = &snapshot.pull_request;
    let json = serde_json::to_string(snapshot)?;
    let tx = begin(conn)?;
    upsert_pr(&tx, pr, now)?;
    let key = PrKey::of(pr);
    let cols = key.cols();
    tx.prepare_cached(
        "INSERT INTO snapshots (host, repo, number, head_sha, fetched_at, json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (host, repo, number) DO UPDATE SET
             head_sha = excluded.head_sha,
             fetched_at = excluded.fetched_at,
             json = excluded.json",
    )?
    .execute(params![
        cols.host,
        cols.repo,
        cols.number,
        pr.head.sha.as_str(),
        now,
        json,
    ])?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn read_snapshot(conn: &Connection, key: &PrKey) -> Result<Option<CachedSnapshot>> {
    let cols = key.cols();
    let mut stmt = conn.prepare_cached(
        "SELECT json, head_sha, fetched_at FROM snapshots
         WHERE host = ?1 AND repo = ?2 AND number = ?3",
    )?;
    let mut rows = stmt.query(params![cols.host, cols.repo, cols.number])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(CachedSnapshot {
        snapshot: json_col(row, 0)?,
        head_sha: Sha::new(row.get::<_, String>(1)?),
        fetched_at: from_ms(row.get(2)?),
    }))
}
