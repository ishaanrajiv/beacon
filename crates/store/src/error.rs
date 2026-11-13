//! The store's error: what failed, and how to fix it, as problems; and which answer it is.

use beacon_core::Problem;
use rusqlite::ErrorCode;
use std::fmt;

/// Which answer an error is (`spec/api.md`, "Errors").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// `400`: a value the request can't use (a key, a label, a context, a state).
    Invalid,
    /// `404`: no such item, silence, source or device.
    NotFound,
    /// `409`: an Idempotency-Key reused with another body, a snapshot out of order, an action
    /// that doesn't apply, a reserved source.
    Conflict,
    /// `422`: an undeclared kind, or facts and slots that don't fit it.
    Undeclared,
    /// `503`: storage failed; retry.
    Storage,
}

impl ErrorKind {
    /// The HTTP status.
    pub fn status(self) -> u16 {
        match self {
            ErrorKind::Invalid => 400,
            ErrorKind::NotFound => 404,
            ErrorKind::Conflict => 409,
            ErrorKind::Undeclared => 422,
            ErrorKind::Storage => 503,
        }
    }
}

/// An error with every problem found; the first is the API's `error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub problems: Vec<Problem>,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(
        kind: ErrorKind,
        at: impl Into<String>,
        message: impl Into<String>,
        fix: impl Into<String>,
    ) -> Error {
        Error {
            kind,
            problems: vec![Problem::new(at, message, fix)],
        }
    }

    pub fn many(kind: ErrorKind, problems: Vec<Problem>) -> Error {
        Error { kind, problems }
    }

    /// The first problem.
    pub fn problem(&self) -> Problem {
        self.problems.first().cloned().unwrap_or_else(|| {
            Problem::new("beacon.db", "an error with no description", "report it as a bug")
        })
    }

    pub(crate) fn not_found(at: &str, what: String) -> Error {
        Error::new(
            ErrorKind::NotFound,
            at,
            what,
            "check the id; list them with the matching GET route",
        )
    }

    pub(crate) fn sql(doing: &str, e: &rusqlite::Error) -> Error {
        let fix = match e.sqlite_error_code() {
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
                "another program holds a lock on beacon.db; stop it (only one Beacon may use a data_dir), or retry"
            }
            Some(ErrorCode::DiskFull) => "free space on the disk that holds data_dir",
            Some(ErrorCode::ReadOnly | ErrorCode::CannotOpen | ErrorCode::PermissionDenied) => {
                "make data_dir and beacon.db writable by the user Beacon runs as"
            }
            Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => {
                "beacon.db is damaged: stop Beacon, move beacon.db aside and restore it from a backup"
            }
            _ => "if this repeats, report it as a bug in beacon-store with this message",
        };
        Error::new(ErrorKind::Storage, "beacon.db", format!("{doing}: {e}"), fix)
    }

    pub(crate) fn json(doing: &str, e: &serde_json::Error) -> Error {
        Error::new(
            ErrorKind::Storage,
            "beacon.db",
            format!("{doing}: {e}"),
            "a row was not written by this version of Beacon; restore a backup or report it as a bug",
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lines: Vec<String> = self.problems.iter().map(ToString::to_string).collect();
        write!(f, "{}", lines.join("\n"))
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Error {
        Error::sql("query failed", &e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Error {
        Error::json("a stored value can't be read", &e)
    }
}
