use std::collections::HashMap;

use forge::model::Timestamp;
use rusqlite::{Connection, params};

use crate::clock::from_ms;
use crate::error::Result;
use crate::keys::{PrKey, begin};

/// The files of one pull request the reviewer marked viewed, each at the
/// blob it had then. A file whose blob changed is not viewed any more, and
/// goes back to viewed if the blob returns to the one that was marked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewedSet {
    by_path: HashMap<String, Vec<(String, Timestamp)>>,
}

impl ViewedSet {
    #[must_use]
    pub fn is_viewed(&self, path: &str, blob_id: &str) -> bool {
        self.viewed_at(path, blob_id).is_some()
    }

    #[must_use]
    pub fn viewed_at(&self, path: &str, blob_id: &str) -> Option<Timestamp> {
        self.by_path
            .get(path)?
            .iter()
            .find(|(blob, _)| blob == blob_id)
            .map(|&(_, at)| at)
    }

    /// Number of (path, blob) marks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_path.values().map(Vec::len).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

/// Marks or unmarks `(path, blob_id)` pairs in one transaction.
pub(crate) fn set_viewed(
    conn: &mut Connection,
    now: i64,
    key: &PrKey,
    files: &[(String, String)],
    viewed: bool,
) -> Result<()> {
    let cols = key.cols();
    let tx = begin(conn)?;
    if viewed {
        let mut mark = tx.prepare_cached(
            "INSERT INTO viewed_files (host, repo, number, path, blob_id, viewed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (host, repo, number, path, blob_id)
             DO UPDATE SET viewed_at = excluded.viewed_at",
        )?;
        for (path, blob_id) in files {
            mark.execute(params![
                cols.host,
                cols.repo,
                cols.number,
                path,
                blob_id,
                now
            ])?;
        }
    } else {
        let mut unmark = tx.prepare_cached(
            "DELETE FROM viewed_files
             WHERE host = ?1 AND repo = ?2 AND number = ?3 AND path = ?4 AND blob_id = ?5",
        )?;
        for (path, blob_id) in files {
            unmark.execute(params![cols.host, cols.repo, cols.number, path, blob_id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn read_viewed(conn: &Connection, key: &PrKey) -> Result<ViewedSet> {
    let cols = key.cols();
    let mut stmt = conn.prepare_cached(
        "SELECT path, blob_id, viewed_at FROM viewed_files
         WHERE host = ?1 AND repo = ?2 AND number = ?3",
    )?;
    let mut rows = stmt.query(params![cols.host, cols.repo, cols.number])?;
    let mut set = ViewedSet::default();
    while let Some(row) = rows.next()? {
        set.by_path
            .entry(row.get(0)?)
            .or_default()
            .push((row.get(1)?, from_ms(row.get(2)?)));
    }
    Ok(set)
}
