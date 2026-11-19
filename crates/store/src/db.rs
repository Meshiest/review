//! Opening the database file: pragmas, migrations, and what to do with a
//! file that cannot be used.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::DateTime;
use rusqlite::{Connection, OpenFlags};

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::migrations;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The one connection that writes. WAL lets readers run during its
/// transactions; `synchronous=NORMAL` skips the fsync per commit, which can
/// lose the last transactions on power loss but never corrupts the file.
pub(crate) fn open_writer(path: &Path, quick_check_max_bytes: u64) -> Result<Connection> {
    let existing = fs::metadata(path).map_or(0, |meta| meta.len());
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // Only a fresh file can still choose its vacuum mode. A garbage file
    // fails at the next pragma as NotADatabase, which is how it is found.
    if existing == 0 {
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    }
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    migrations::migrate(&mut conn)?;
    if existing > 0 && existing <= quick_check_max_bytes {
        let verdict: String = conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
        if verdict != "ok" {
            return Err(Error::Corrupt(verdict));
        }
    }
    Ok(conn)
}

pub(crate) fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -16_384)?;
    Ok(conn)
}

/// Opens the writer; a corrupt or newer-version file is renamed (with its
/// WAL and shared-memory files) and a fresh database created in its place.
pub(crate) fn open_or_replace(
    path: &Path,
    clock: &dyn Clock,
    quick_check_max_bytes: u64,
) -> Result<(Connection, Option<PathBuf>)> {
    match open_writer(path, quick_check_max_bytes) {
        Ok(conn) => Ok((conn, None)),
        Err(err) => {
            let Some(reason) = err.unusable_reason() else {
                return Err(err);
            };
            let aside = move_aside(path, reason, clock)?;
            let conn = open_writer(path, quick_check_max_bytes)?;
            Ok((conn, Some(aside)))
        }
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(suffix);
    PathBuf::from(os)
}

/// `name.<reason>-<UTC time>` next to the file; the WAL and `-shm` files go
/// along, since a stale WAL applied to a new database would corrupt it.
fn move_aside(path: &Path, reason: &str, clock: &dyn Clock) -> Result<PathBuf> {
    let stamp = DateTime::from_timestamp_millis(clock.now_ms())
        .unwrap_or_default()
        .format("%Y%m%dT%H%M%S");
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut target = path.with_file_name(format!("{name}.{reason}-{stamp}"));
    let mut attempt = 1;
    while target.exists() {
        target = path.with_file_name(format!("{name}.{reason}-{stamp}-{attempt}"));
        attempt += 1;
    }
    fs::rename(path, &target)?;
    for suffix in ["-wal", "-shm"] {
        let from = with_suffix(path, suffix);
        if from.exists() {
            fs::rename(&from, with_suffix(&target, suffix))?;
        }
    }
    Ok(target)
}
