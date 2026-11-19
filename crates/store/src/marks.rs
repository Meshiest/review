//! Local review tracking: what the user last saw of each pull request,
//! whether they marked it reviewed, and how much of it they viewed. None of
//! it is ever sent to a forge.

use forge::model::{PrNumber, RepoRef, Timestamp};
use rusqlite::{Connection, params};

use crate::clock::{from_ms, to_ms};
use crate::error::Result;
use crate::keys::{PrKey, begin};

/// The pull request as it was when the user last opened it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The head commit then, as the inbox row shows it.
    pub head: String,
    pub comments: u32,
    /// Submitted reviews then.
    pub reviews: u32,
    pub at: Timestamp,
}

/// The user marked it reviewed, at this head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reviewed {
    pub head: String,
    pub at: Timestamp,
}

/// How many of its files were viewed when it was last open, at this head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewedProgress {
    pub head: String,
    pub viewed: u32,
    pub total: u32,
}

/// Everything tracked locally about one pull request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewMark {
    pub seen: Option<Seen>,
    /// Marked unread by hand; it stays unread until it is opened again.
    pub unread: bool,
    pub reviewed: Option<Reviewed>,
    pub progress: Option<ViewedProgress>,
}

impl ReviewMark {
    /// Nothing tracked: the row is dropped rather than stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

fn count_col(n: u32) -> i64 {
    i64::from(n)
}

fn count_of(n: Option<i64>) -> u32 {
    n.and_then(|n| u32::try_from(n).ok()).unwrap_or(0)
}

/// Replaces `key`'s marks; an empty mark deletes the row.
pub(crate) fn put_mark(
    conn: &mut Connection,
    now: i64,
    key: &PrKey,
    mark: &ReviewMark,
) -> Result<()> {
    let cols = key.cols();
    let tx = begin(conn)?;
    if mark.is_empty() {
        tx.execute(
            "DELETE FROM review_marks WHERE host = ?1 AND repo = ?2 AND number = ?3",
            params![cols.host, cols.repo, cols.number],
        )?;
    } else {
        let seen = mark.seen.as_ref();
        let reviewed = mark.reviewed.as_ref();
        let progress = mark.progress.as_ref();
        tx.execute(
            "INSERT INTO review_marks (host, repo, number, seen_head, seen_comments,
                 seen_reviews, seen_at, unread, reviewed_head, reviewed_at, viewed_head,
                 viewed_count, viewed_total, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT (host, repo, number) DO UPDATE SET
                 seen_head = excluded.seen_head, seen_comments = excluded.seen_comments,
                 seen_reviews = excluded.seen_reviews, seen_at = excluded.seen_at,
                 unread = excluded.unread, reviewed_head = excluded.reviewed_head,
                 reviewed_at = excluded.reviewed_at, viewed_head = excluded.viewed_head,
                 viewed_count = excluded.viewed_count, viewed_total = excluded.viewed_total,
                 updated_at = excluded.updated_at",
            params![
                cols.host,
                cols.repo,
                cols.number,
                seen.map(|s| s.head.as_str()),
                seen.map(|s| count_col(s.comments)),
                seen.map(|s| count_col(s.reviews)),
                seen.map(|s| to_ms(s.at)),
                mark.unread,
                reviewed.map(|r| r.head.as_str()),
                reviewed.map(|r| to_ms(r.at)),
                progress.map(|p| p.head.as_str()),
                progress.map(|p| count_col(p.viewed)),
                progress.map(|p| count_col(p.total)),
                now,
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// `owner/name` back into a repository; a GitLab owner may hold slashes.
fn repo_of(host: String, path: &str) -> RepoRef {
    let (owner, name) = path.rsplit_once('/').unwrap_or(("", path));
    RepoRef::new(host, owner, name)
}

/// Every pull request with marks.
pub(crate) fn read_marks(conn: &Connection) -> Result<Vec<(PrKey, ReviewMark)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT host, repo, number, seen_head, seen_comments, seen_reviews, seen_at, unread,
                reviewed_head, reviewed_at, viewed_head, viewed_count, viewed_total
         FROM review_marks",
    )?;
    let mut rows = stmt.query([])?;
    let mut marks = Vec::new();
    while let Some(row) = rows.next()? {
        let path: String = row.get(1)?;
        let number: i64 = row.get(2)?;
        let key = PrKey::new(
            repo_of(row.get(0)?, &path),
            PrNumber(u64::try_from(number).unwrap_or_default()),
        );
        let seen = row
            .get::<_, Option<String>>(3)?
            .map(|head| -> Result<Seen> {
                Ok(Seen {
                    head,
                    comments: count_of(row.get(4)?),
                    reviews: count_of(row.get(5)?),
                    at: from_ms(row.get::<_, Option<i64>>(6)?.unwrap_or_default()),
                })
            });
        let reviewed = row
            .get::<_, Option<String>>(8)?
            .map(|head| -> Result<Reviewed> {
                Ok(Reviewed {
                    head,
                    at: from_ms(row.get::<_, Option<i64>>(9)?.unwrap_or_default()),
                })
            });
        let progress = row
            .get::<_, Option<String>>(10)?
            .map(|head| -> Result<ViewedProgress> {
                Ok(ViewedProgress {
                    head,
                    viewed: count_of(row.get(11)?),
                    total: count_of(row.get(12)?),
                })
            });
        let mark = ReviewMark {
            seen: seen.transpose()?,
            unread: row.get(7)?,
            reviewed: reviewed.transpose()?,
            progress: progress.transpose()?,
        };
        marks.push((key, mark));
    }
    Ok(marks)
}
