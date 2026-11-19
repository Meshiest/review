use forge::model::{CommentId, LineRef, Sha, ThreadId, Timestamp};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::clock::from_ms;
use crate::error::Result;
use crate::keys::{PrKey, begin};

/// What an unsent comment is attached to. Together with the pull request it
/// is the draft's key, so it names a place, not a commit: the draft survives
/// a new head (see [`Draft::commit`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftAnchor {
    /// The review's summary body.
    Review,
    /// A reply in an existing thread.
    Reply { thread: ThreadId },
    /// A new inline comment on a line, or on a range starting at `start`.
    Line {
        path: String,
        line: LineRef,
        start: Option<LineRef>,
    },
    /// A new comment on a whole file.
    File { path: String },
    /// A new top-level comment on the pull request.
    Conversation,
    /// New text for the queued comment with this id.
    Edit { queued: i64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub pr: PrKey,
    pub anchor: DraftAnchor,
    pub body: String,
    /// The head commit the draft was written against, when it has a line.
    pub commit: Option<Sha>,
    pub updated_at: Timestamp,
}

fn anchor_col(anchor: &DraftAnchor) -> Result<String> {
    Ok(serde_json::to_string(anchor)?)
}

pub(crate) fn put_draft(
    conn: &mut Connection,
    now: i64,
    pr: &PrKey,
    anchor: &DraftAnchor,
    body: &str,
    commit: Option<&Sha>,
) -> Result<()> {
    let cols = pr.cols();
    let anchor = anchor_col(anchor)?;
    let tx = begin(conn)?;
    tx.prepare_cached(
        "INSERT INTO drafts (host, repo, number, anchor, body, commit_sha, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (host, repo, number, anchor) DO UPDATE SET
             body = excluded.body,
             commit_sha = excluded.commit_sha,
             updated_at = excluded.updated_at",
    )?
    .execute(params![
        cols.host,
        cols.repo,
        cols.number,
        anchor,
        body,
        commit.map(Sha::as_str),
        now,
    ])?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn delete_draft(conn: &mut Connection, pr: &PrKey, anchor: &DraftAnchor) -> Result<()> {
    let cols = pr.cols();
    let anchor = anchor_col(anchor)?;
    let tx = begin(conn)?;
    tx.execute(
        "DELETE FROM drafts WHERE host = ?1 AND repo = ?2 AND number = ?3 AND anchor = ?4",
        params![cols.host, cols.repo, cols.number, anchor],
    )?;
    tx.commit()?;
    Ok(())
}

fn draft_from_row(pr: &PrKey, row: &rusqlite::Row<'_>) -> Result<Draft> {
    let anchor: String = row.get(0)?;
    Ok(Draft {
        pr: pr.clone(),
        anchor: serde_json::from_str(&anchor)?,
        body: row.get(1)?,
        commit: row.get::<_, Option<String>>(2)?.map(Sha::new),
        updated_at: from_ms(row.get(3)?),
    })
}

pub(crate) fn read_draft(
    conn: &Connection,
    pr: &PrKey,
    anchor: &DraftAnchor,
) -> Result<Option<Draft>> {
    let cols = pr.cols();
    let anchor = anchor_col(anchor)?;
    let mut stmt = conn.prepare_cached(
        "SELECT anchor, body, commit_sha, updated_at FROM drafts
         WHERE host = ?1 AND repo = ?2 AND number = ?3 AND anchor = ?4",
    )?;
    let mut rows = stmt.query(params![cols.host, cols.repo, cols.number, anchor])?;
    rows.next()?.map(|row| draft_from_row(pr, row)).transpose()
}

/// Every draft of the pull request, oldest edit first.
pub(crate) fn read_drafts(conn: &Connection, pr: &PrKey) -> Result<Vec<Draft>> {
    let cols = pr.cols();
    let mut stmt = conn.prepare_cached(
        "SELECT anchor, body, commit_sha, updated_at FROM drafts
         WHERE host = ?1 AND repo = ?2 AND number = ?3
         ORDER BY updated_at, anchor",
    )?;
    let mut rows = stmt.query(params![cols.host, cols.repo, cols.number])?;
    let mut drafts = Vec::new();
    while let Some(row) = rows.next()? {
        drafts.push(draft_from_row(pr, row)?);
    }
    Ok(drafts)
}

/// A comment added to the pending review: written, accepted by the forge
/// side (or the local queue) and waiting for the review to be submitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedComment {
    pub id: i64,
    pub pr: PrKey,
    pub anchor: DraftAnchor,
    pub body: String,
    pub commit: Option<Sha>,
    pub queued_at: Timestamp,
    /// The forge's id of the comment in its pending review; `None` while
    /// only the local queue has it.
    pub forge_id: Option<CommentId>,
    /// The thread it opened, where the forge makes threads before the review
    /// is submitted (GitHub).
    pub thread: Option<ThreadId>,
}

/// What a new queued comment holds, before the store gives it an id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewQueued {
    pub anchor: DraftAnchor,
    pub body: String,
    pub commit: Option<Sha>,
    pub forge_id: Option<CommentId>,
    pub thread: Option<ThreadId>,
}

fn insert_queued(
    tx: &rusqlite::Transaction<'_>,
    now: i64,
    pr: &PrKey,
    new: &NewQueued,
) -> Result<QueuedComment> {
    let cols = pr.cols();
    tx.execute(
        "INSERT INTO queued_comments
             (host, repo, number, anchor, body, commit_sha, queued_at, forge_id, thread_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            cols.host,
            cols.repo,
            cols.number,
            anchor_col(&new.anchor)?,
            new.body,
            new.commit.as_ref().map(Sha::as_str),
            now,
            new.forge_id.as_ref().map(CommentId::as_str),
            new.thread.as_ref().map(ThreadId::as_str),
        ],
    )?;
    Ok(QueuedComment {
        id: tx.last_insert_rowid(),
        pr: pr.clone(),
        anchor: new.anchor.clone(),
        body: new.body.clone(),
        commit: new.commit.clone(),
        queued_at: from_ms(now),
        forge_id: new.forge_id.clone(),
        thread: new.thread.clone(),
    })
}

/// Moves a draft into the pending review in one transaction, so a crash
/// leaves it as a draft or as a queued comment, never both or neither.
pub(crate) fn queue_comment(
    conn: &mut Connection,
    now: i64,
    pr: &PrKey,
    new: &NewQueued,
) -> Result<QueuedComment> {
    let cols = pr.cols();
    let anchor = anchor_col(&new.anchor)?;
    let tx = begin(conn)?;
    let queued = insert_queued(&tx, now, pr, new)?;
    tx.execute(
        "DELETE FROM drafts WHERE host = ?1 AND repo = ?2 AND number = ?3 AND anchor = ?4",
        params![cols.host, cols.repo, cols.number, anchor],
    )?;
    tx.commit()?;
    Ok(queued)
}

/// Records comments the forge's pending review has that this store does not
/// (a review started on the web or in another session), leaving drafts
/// alone. Returns every queued comment of the pull request afterwards.
pub(crate) fn adopt_queued(
    conn: &mut Connection,
    now: i64,
    pr: &PrKey,
    found: &[NewQueued],
) -> Result<Vec<QueuedComment>> {
    let cols = pr.cols();
    let tx = begin(conn)?;
    for new in found {
        let Some(forge_id) = &new.forge_id else {
            continue;
        };
        let known: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM queued_comments
                 WHERE host = ?1 AND repo = ?2 AND number = ?3 AND forge_id = ?4)",
            params![cols.host, cols.repo, cols.number, forge_id.as_str()],
            |row| row.get(0),
        )?;
        if !known {
            insert_queued(&tx, now, pr, new)?;
        }
    }
    tx.commit()?;
    read_queued(conn, pr)
}

/// Drops one queued comment (deleted from the review, or gone on the forge).
pub(crate) fn delete_queued(conn: &mut Connection, id: i64) -> Result<()> {
    let tx = begin(conn)?;
    tx.execute("DELETE FROM queued_comments WHERE id = ?1", params![id])?;
    tx.commit()?;
    Ok(())
}

/// A queued comment's new text, and its new forge id when editing it meant
/// deleting and adding it again.
pub(crate) fn update_queued(
    conn: &mut Connection,
    id: i64,
    body: &str,
    forge_id: Option<&CommentId>,
) -> Result<()> {
    let tx = begin(conn)?;
    tx.execute(
        "UPDATE queued_comments SET body = ?2, forge_id = COALESCE(?3, forge_id) WHERE id = ?1",
        params![id, body, forge_id.map(CommentId::as_str)],
    )?;
    tx.commit()?;
    Ok(())
}

/// The pending review's comments, in the order they were added.
pub(crate) fn read_queued(conn: &Connection, pr: &PrKey) -> Result<Vec<QueuedComment>> {
    let cols = pr.cols();
    let mut stmt = conn.prepare_cached(
        "SELECT id, anchor, body, commit_sha, queued_at, forge_id, thread_id FROM queued_comments
         WHERE host = ?1 AND repo = ?2 AND number = ?3 ORDER BY id",
    )?;
    let mut rows = stmt.query(params![cols.host, cols.repo, cols.number])?;
    let mut queued = Vec::new();
    while let Some(row) = rows.next()? {
        let anchor: String = row.get(1)?;
        queued.push(QueuedComment {
            id: row.get(0)?,
            pr: pr.clone(),
            anchor: serde_json::from_str(&anchor)?,
            body: row.get(2)?,
            commit: row.get::<_, Option<String>>(3)?.map(Sha::new),
            queued_at: from_ms(row.get(4)?),
            forge_id: row.get::<_, Option<String>>(5)?.map(CommentId::new),
            thread: row.get::<_, Option<String>>(6)?.map(ThreadId::new),
        });
    }
    Ok(queued)
}

/// The review went out: its queued comments and its summary draft are done.
pub(crate) fn finish_review(conn: &mut Connection, pr: &PrKey) -> Result<()> {
    let cols = pr.cols();
    let summary = anchor_col(&DraftAnchor::Review)?;
    let tx = begin(conn)?;
    tx.execute(
        "DELETE FROM queued_comments WHERE host = ?1 AND repo = ?2 AND number = ?3",
        params![cols.host, cols.repo, cols.number],
    )?;
    tx.execute(
        "DELETE FROM drafts WHERE host = ?1 AND repo = ?2 AND number = ?3 AND anchor = ?4",
        params![cols.host, cols.repo, cols.number, summary],
    )?;
    tx.commit()?;
    Ok(())
}
