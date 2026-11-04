//! What a store operation can fail with, split by what the caller may
//! conclude about a write: refused and failed writes are known unapplied, a
//! commit that errored may or may not have landed.

/// A store failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The request was refused before anything was written: an invalid
    /// document, a store this code cannot open, a schema newer than it knows.
    /// Known unapplied.
    Refused(String),
    /// The run uuid is already stored with different content. Known
    /// unapplied.
    Conflict { uuid: String },
    /// The store failed before the commit was attempted (an I/O or SQLite
    /// error, stored data that does not read back). Known unapplied.
    Failed(String),
    /// The commit itself errored: the write may or may not have landed. A
    /// retry under the same uuid settles it.
    CommitUncertain(String),
}

impl StoreError {
    /// Whether the write's outcome is unknown; every other error is known
    /// unapplied.
    pub fn outcome_uncertain(&self) -> bool {
        matches!(self, Self::CommitUncertain(_))
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(why) => write!(f, "bench store refused the request: {why}"),
            Self::Conflict { uuid } => write!(
                f,
                "bench store already holds run {uuid} with different content"
            ),
            Self::Failed(why) => write!(f, "bench store failed: {why}"),
            Self::CommitUncertain(why) => write!(
                f,
                "bench store commit failed and the write may have landed (retry with the same run uuid to settle it): {why}"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Failed(error.to_string())
    }
}
