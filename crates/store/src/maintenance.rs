//! Pruning and the size check. Drafts, queued review comments and
//! subscriptions are user data and are never pruned.

use std::time::Duration;

use rusqlite::Connection;

use crate::error::Result;
use crate::keys::begin;

/// What one maintenance pass removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// Closed or merged pull requests not seen for the retention period.
    pub pruned_pull_requests: usize,
    /// Inbox sections not refreshed for the retention period.
    pub pruned_sections: usize,
    /// Snapshots dropped because the database was over its size limit.
    pub trimmed_snapshots: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub retention: Duration,
    pub max_bytes: u64,
}

/// Bytes in use by pages, not counting free pages.
pub(crate) fn used_bytes(conn: &Connection) -> Result<u64> {
    let pages: i64 = conn.pragma_query_value(None, "page_count", |row| row.get(0))?;
    let free: i64 = conn.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    let page_size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
    Ok(u64::try_from((pages - free).max(0) * page_size).unwrap_or(0))
}

/// Drops closed or merged pull requests seen before `cutoff_ms` that no
/// section lists, with their snapshots, viewed files and review marks. With
/// `sections`, first drops the sections not refreshed since the cutoff, which
/// is what frees their pull requests. Returns (pull requests, sections) removed.
fn prune(conn: &mut Connection, cutoff_ms: i64, sections: bool) -> Result<(usize, usize)> {
    const DOOMED: &str = "SELECT p.host, p.repo, p.number FROM pull_requests p
         WHERE p.state IN ('merged', 'closed') AND p.last_seen_at < ?1
           AND NOT EXISTS (SELECT 1 FROM inbox_items i WHERE i.pr_id = p.id)";
    let tx = begin(conn)?;
    let mut dropped_sections = 0;
    if sections {
        tx.execute(
            "DELETE FROM inbox_items WHERE section_id IN
                 (SELECT id FROM inbox_sections WHERE fetched_at < ?1)",
            [cutoff_ms],
        )?;
        dropped_sections = tx.execute(
            "DELETE FROM inbox_sections WHERE fetched_at < ?1",
            [cutoff_ms],
        )?;
    }
    // Dependents first: the predicate reads `pull_requests`.
    let mut pull_requests = 0;
    for table in ["snapshots", "viewed_files", "review_marks", "pull_requests"] {
        pull_requests = tx.execute(
            &format!("DELETE FROM {table} WHERE (host, repo, number) IN ({DOOMED})"),
            [cutoff_ms],
        )?;
    }
    tx.commit()?;
    Ok((pull_requests, dropped_sections))
}

/// Drops the oldest quarter of the snapshots (at least one); returns how
/// many went, 0 when none are left.
fn trim_snapshots(conn: &mut Connection) -> Result<usize> {
    let tx = begin(conn)?;
    let removed = tx.execute(
        "DELETE FROM snapshots WHERE (host, repo, number) IN
             (SELECT host, repo, number FROM snapshots
              ORDER BY fetched_at LIMIT max(1, (SELECT count(*) FROM snapshots) / 4))",
        [],
    )?;
    tx.commit()?;
    Ok(removed)
}

/// Retention pruning, then, while the database is over `max_bytes`,
/// snapshots oldest first, and finally every closed or merged pull request
/// no section lists.
pub(crate) fn maintain(
    conn: &mut Connection,
    now: i64,
    limits: &Limits,
) -> Result<MaintenanceReport> {
    const MAX_TRIM_ROUNDS: usize = 64;
    let retention_ms = i64::try_from(limits.retention.as_millis()).unwrap_or(i64::MAX);
    let mut report = MaintenanceReport {
        bytes_before: used_bytes(conn)?,
        ..MaintenanceReport::default()
    };
    let (pull_requests, sections) = prune(conn, now.saturating_sub(retention_ms), true)?;
    report.pruned_pull_requests = pull_requests;
    report.pruned_sections = sections;

    for _ in 0..MAX_TRIM_ROUNDS {
        if used_bytes(conn)? <= limits.max_bytes {
            break;
        }
        let removed = trim_snapshots(conn)?;
        report.trimmed_snapshots += removed;
        if removed == 0 {
            report.pruned_pull_requests += prune(conn, i64::MAX, false)?.0;
            break;
        }
    }
    reclaim(conn)?;
    report.bytes_after = used_bytes(conn)?;
    Ok(report)
}

/// Returns freed pages to the file (the database is created with
/// incremental auto-vacuum) and shrinks the WAL.
fn reclaim(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(PASSIVE);")?;
    Ok(())
}
