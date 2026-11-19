use forge::model::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

use crate::clock::{from_ms, to_ms};
use crate::error::Result;
use crate::keys::begin;

/// What conditional requests and incremental fetches need to remember about
/// one endpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchMeta {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Newest `updated_at` seen, to ask only for what changed since.
    pub watermark: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredFetchMeta {
    pub meta: FetchMeta,
    /// When the store was told about this fetch.
    pub fetched_at: Timestamp,
}

pub(crate) fn set_meta(
    conn: &mut Connection,
    now: i64,
    endpoint: &str,
    meta: &FetchMeta,
) -> Result<()> {
    let tx = begin(conn)?;
    tx.execute(
        "INSERT INTO fetch_meta (endpoint, etag, last_modified, watermark, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (endpoint) DO UPDATE SET
             etag = excluded.etag,
             last_modified = excluded.last_modified,
             watermark = excluded.watermark,
             fetched_at = excluded.fetched_at",
        params![
            endpoint,
            meta.etag,
            meta.last_modified,
            meta.watermark.map(to_ms),
            now
        ],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn read_meta(conn: &Connection, endpoint: &str) -> Result<Option<StoredFetchMeta>> {
    Ok(conn
        .prepare_cached(
            "SELECT etag, last_modified, watermark, fetched_at FROM fetch_meta WHERE endpoint = ?1",
        )?
        .query_row([endpoint], |row| {
            Ok(StoredFetchMeta {
                meta: FetchMeta {
                    etag: row.get(0)?,
                    last_modified: row.get(1)?,
                    watermark: row.get::<_, Option<i64>>(2)?.map(from_ms),
                },
                fetched_at: from_ms(row.get(3)?),
            })
        })
        .optional()?)
}
