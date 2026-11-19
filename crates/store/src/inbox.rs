//! Pull requests and the inbox sections that list them.

use forge::model::{InboxSection, PullRequest, Timestamp};
use rusqlite::{Connection, OptionalExtension, params};

use crate::clock::{from_ms, to_ms};
use crate::error::Result;
use crate::keys::{PrKey, begin, json_col, state_col};

/// An inbox as last fetched.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedInbox {
    /// In the order the forge returned them (most recently updated first).
    pub prs: Vec<PullRequest>,
    pub fetched_at: Timestamp,
}

fn section_key(section: &InboxSection) -> Result<String> {
    Ok(serde_json::to_string(section)?)
}

/// Writes or replaces one pull request. A copy with an older `updated_at`
/// than the cached one (a slow response that lost the race) only refreshes
/// `last_seen_at`. Fields the source did not report (`ci`, `diff_stat`) are
/// kept from the cached copy while the head commit is the same.
pub(crate) fn upsert_pr(conn: &Connection, pr: &PullRequest, now: i64) -> Result<i64> {
    let key = PrKey::of(pr);
    let cols = key.cols();
    let merged;
    let pr = if pr.ci.is_none() || pr.diff_stat.is_none() {
        match read_pr(conn, &key)? {
            Some(old) if old.head.sha == pr.head.sha => {
                let mut copy = pr.clone();
                copy.ci = copy.ci.or(old.ci);
                copy.diff_stat = copy.diff_stat.or(old.diff_stat);
                merged = copy;
                &merged
            }
            _ => pr,
        }
    } else {
        pr
    };
    let json = serde_json::to_string(pr)?;
    let id = conn
        .prepare_cached(
            "INSERT INTO pull_requests (host, repo, number, state, updated_at, last_seen_at, json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (host, repo, number) DO UPDATE SET
             state = CASE WHEN excluded.updated_at >= updated_at THEN excluded.state ELSE state END,
             json = CASE WHEN excluded.updated_at >= updated_at THEN excluded.json ELSE json END,
             updated_at = max(excluded.updated_at, updated_at),
             last_seen_at = excluded.last_seen_at
         RETURNING id",
        )?
        .query_row(
            params![
                cols.host,
                cols.repo,
                cols.number,
                state_col(pr.state),
                to_ms(pr.updated_at),
                now,
                json,
            ],
            |row| row.get(0),
        )?;
    Ok(id)
}

pub(crate) fn read_pr(conn: &Connection, key: &PrKey) -> Result<Option<PullRequest>> {
    let cols = key.cols();
    let mut stmt = conn.prepare_cached(
        "SELECT json FROM pull_requests WHERE host = ?1 AND repo = ?2 AND number = ?3",
    )?;
    let mut rows = stmt.query(params![cols.host, cols.repo, cols.number])?;
    rows.next()?.map(|row| json_col(row, 0)).transpose()
}

/// Replaces the section's list in one transaction.
pub(crate) fn put_inbox(
    conn: &mut Connection,
    now: i64,
    host: &str,
    section: &InboxSection,
    prs: &[PullRequest],
) -> Result<()> {
    let section = section_key(section)?;
    let tx = begin(conn)?;
    tx.execute(
        "INSERT INTO inbox_sections (host, section, fetched_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (host, section) DO UPDATE SET fetched_at = excluded.fetched_at",
        params![host, section, now],
    )?;
    let id: i64 = tx.query_row(
        "SELECT id FROM inbox_sections WHERE host = ?1 AND section = ?2",
        params![host, section],
        |row| row.get(0),
    )?;
    tx.execute("DELETE FROM inbox_items WHERE section_id = ?1", [id])?;
    {
        let mut item = tx.prepare_cached(
            "INSERT INTO inbox_items (section_id, position, pr_id) VALUES (?1, ?2, ?3)",
        )?;
        for (position, pr) in prs.iter().enumerate() {
            let pr_id = upsert_pr(&tx, pr, now)?;
            item.execute(params![
                id,
                i64::try_from(position).unwrap_or(i64::MAX),
                pr_id
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// `None` when the section was never fetched.
pub(crate) fn read_inbox(
    conn: &Connection,
    host: &str,
    section: &InboxSection,
) -> Result<Option<CachedInbox>> {
    let section = section_key(section)?;
    let head: Option<(i64, i64)> = conn
        .prepare_cached(
            "SELECT id, fetched_at FROM inbox_sections WHERE host = ?1 AND section = ?2",
        )?
        .query_row(params![host, section], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let Some((id, fetched_at)) = head else {
        return Ok(None);
    };
    let mut stmt = conn.prepare_cached(
        "SELECT p.json FROM inbox_items i
         JOIN pull_requests p ON p.id = i.pr_id
         WHERE i.section_id = ?1
         ORDER BY i.position",
    )?;
    let mut rows = stmt.query([id])?;
    let mut prs = Vec::new();
    while let Some(row) = rows.next()? {
        prs.push(json_col(row, 0)?);
    }
    Ok(Some(CachedInbox {
        prs,
        fetched_at: from_ms(fetched_at),
    }))
}
