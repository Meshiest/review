use rusqlite::ErrorCode;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors carry text so they are `Clone` and can be handed to the UI.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(String),

    /// The file is not a database or its pages are damaged.
    #[error("database file is corrupt: {0}")]
    Corrupt(String),

    /// Written by a newer build; this one cannot read it safely.
    #[error("database schema version {found} is newer than the supported {supported}")]
    NewerSchema { found: i64, supported: i64 },

    #[error("cached JSON could not be read: {0}")]
    Json(String),

    #[error("i/o error: {0}")]
    Io(String),

    /// The store's threads are gone (it was dropped, or a job panicked).
    #[error("the store is shut down")]
    Shutdown,
}

impl Error {
    /// Why the file should be moved aside and recreated, if it should.
    pub(crate) fn unusable_reason(&self) -> Option<&'static str> {
        match self {
            Self::Corrupt(_) => Some("corrupt"),
            Self::NewerSchema { .. } => Some("newer"),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        match err.sqlite_error_code() {
            Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => {
                Self::Corrupt(err.to_string())
            }
            _ => Self::Database(err.to_string()),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}
