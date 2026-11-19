//! Schema versions. `PRAGMA user_version` holds how many entries of
//! [`MIGRATIONS`] have run. Append a new entry for every schema change and
//! never edit an old one.

use rusqlite::{Connection, TransactionBehavior};

use crate::error::{Error, Result};

pub(crate) const MIGRATIONS: &[&str] = &[V1, V2, V3, V4];

#[cfg(test)]
pub(crate) const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Keys are `(host, repo, number)`: `repo` is the forge path `owner/name`
/// (GitLab groups may nest), times are Unix milliseconds. Tables that hold a
/// JSON document are plain rowid tables with a unique key index: in a
/// `WITHOUT ROWID` table the document would sit in the key's b-tree and
/// spill into overflow pages, which made a 100-row inbox read cost 3 ms.
const V1: &str = "
CREATE TABLE pull_requests (
    id           INTEGER PRIMARY KEY,
    host         TEXT    NOT NULL,
    repo         TEXT    NOT NULL,
    number       INTEGER NOT NULL,
    state        TEXT    NOT NULL,
    updated_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    json         TEXT    NOT NULL,
    UNIQUE (host, repo, number)
);
CREATE INDEX pull_requests_state ON pull_requests (state, updated_at DESC);
CREATE INDEX pull_requests_seen ON pull_requests (last_seen_at);

CREATE TABLE inbox_sections (
    id         INTEGER PRIMARY KEY,
    host       TEXT    NOT NULL,
    section    TEXT    NOT NULL,
    fetched_at INTEGER NOT NULL,
    UNIQUE (host, section)
);

CREATE TABLE inbox_items (
    section_id INTEGER NOT NULL,
    position   INTEGER NOT NULL,
    pr_id      INTEGER NOT NULL,
    PRIMARY KEY (section_id, position)
) WITHOUT ROWID;
CREATE INDEX inbox_items_pr ON inbox_items (pr_id);

CREATE TABLE snapshots (
    id         INTEGER PRIMARY KEY,
    host       TEXT    NOT NULL,
    repo       TEXT    NOT NULL,
    number     INTEGER NOT NULL,
    head_sha   TEXT    NOT NULL,
    fetched_at INTEGER NOT NULL,
    json       TEXT    NOT NULL,
    UNIQUE (host, repo, number)
);
CREATE INDEX snapshots_fetched ON snapshots (fetched_at);

CREATE TABLE viewed_files (
    host      TEXT    NOT NULL,
    repo      TEXT    NOT NULL,
    number    INTEGER NOT NULL,
    path      TEXT    NOT NULL,
    blob_id   TEXT    NOT NULL,
    viewed_at INTEGER NOT NULL,
    PRIMARY KEY (host, repo, number, path, blob_id)
) WITHOUT ROWID;

CREATE TABLE drafts (
    host       TEXT    NOT NULL,
    repo       TEXT    NOT NULL,
    number     INTEGER NOT NULL,
    anchor     TEXT    NOT NULL,
    body       TEXT    NOT NULL,
    commit_sha TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (host, repo, number, anchor)
) WITHOUT ROWID;

CREATE TABLE subscriptions (
    target     TEXT    NOT NULL PRIMARY KEY,
    created_at INTEGER NOT NULL,
    state      TEXT
) WITHOUT ROWID;

CREATE TABLE fetch_meta (
    endpoint      TEXT    NOT NULL PRIMARY KEY,
    etag          TEXT,
    last_modified TEXT,
    watermark     INTEGER,
    fetched_at    INTEGER NOT NULL
) WITHOUT ROWID;
";

/// Review comments that were added to the pending review and not yet
/// submitted. Unlike drafts, several can share an anchor.
const V2: &str = "
CREATE TABLE queued_comments (
    id         INTEGER PRIMARY KEY,
    host       TEXT    NOT NULL,
    repo       TEXT    NOT NULL,
    number     INTEGER NOT NULL,
    anchor     TEXT    NOT NULL,
    body       TEXT    NOT NULL,
    commit_sha TEXT,
    queued_at  INTEGER NOT NULL
);
CREATE INDEX queued_comments_pr ON queued_comments (host, repo, number, id);
";

/// A queued comment remembers what the forge calls it once the forge has it
/// (its comment id, and the thread it opened where the forge makes one), so
/// it can be deleted or edited there before the review is submitted.
const V3: &str = "
ALTER TABLE queued_comments ADD COLUMN forge_id TEXT;
ALTER TABLE queued_comments ADD COLUMN thread_id TEXT;
CREATE UNIQUE INDEX queued_comments_forge ON queued_comments (host, repo, number, forge_id)
    WHERE forge_id IS NOT NULL;
";

/// Local review tracking, one row per pull request: the seen mark (head,
/// comment and review counts when last opened), a hand-set unread flag, the
/// reviewed mark and the viewed progress. Never sent to a forge.
const V4: &str = "
CREATE TABLE review_marks (
    host          TEXT    NOT NULL,
    repo          TEXT    NOT NULL,
    number        INTEGER NOT NULL,
    seen_head     TEXT,
    seen_comments INTEGER,
    seen_reviews  INTEGER,
    seen_at       INTEGER,
    unread        INTEGER NOT NULL DEFAULT 0,
    reviewed_head TEXT,
    reviewed_at   INTEGER,
    viewed_head   TEXT,
    viewed_count  INTEGER,
    viewed_total  INTEGER,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (host, repo, number)
) WITHOUT ROWID;
";

/// Brings the database up to the newest entry of `migrations`, each in its
/// own transaction. A newer database is an error, not something to open.
pub(crate) fn migrate_with(conn: &mut Connection, migrations: &[&str]) -> Result<()> {
    let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let supported = migrations.len() as i64;
    if found > supported {
        return Err(Error::NewerSchema { found, supported });
    }
    let done = usize::try_from(found).unwrap_or(0);
    for (index, sql) in migrations.iter().enumerate().skip(done) {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(sql)?;
        tx.pragma_update(
            None,
            "user_version",
            i64::try_from(index + 1).unwrap_or(i64::MAX),
        )?;
        tx.commit()?;
    }
    Ok(())
}

pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    migrate_with(conn, MIGRATIONS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn empty_database_gets_every_step_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(version(&conn), SCHEMA_VERSION);
        migrate(&mut conn).unwrap();
        assert_eq!(version(&conn), SCHEMA_VERSION);
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(tables >= 8, "{tables}");
    }

    #[test]
    fn later_step_runs_over_existing_data() {
        let steps = ["CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (7);"];
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_with(&mut conn, &steps).unwrap();
        let steps2 = [steps[0], "ALTER TABLE t ADD COLUMN b INTEGER DEFAULT 3;"];
        migrate_with(&mut conn, &steps2).unwrap();
        assert_eq!(version(&conn), 2);
        let (a, b): (i64, i64) = conn
            .query_row("SELECT a, b FROM t", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!((a, b), (7, 3));
    }

    #[test]
    fn newer_database_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert_eq!(
            migrate(&mut conn),
            Err(Error::NewerSchema {
                found: SCHEMA_VERSION + 1,
                supported: SCHEMA_VERSION
            })
        );
    }

    #[test]
    fn failed_step_rolls_back() {
        let mut conn = Connection::open_in_memory().unwrap();
        let steps = ["CREATE TABLE t (a); NOT SQL;"];
        assert!(migrate_with(&mut conn, &steps).is_err());
        assert_eq!(version(&conn), 0);
        let tables: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .unwrap();
        assert_eq!(tables, 0);
    }
}
