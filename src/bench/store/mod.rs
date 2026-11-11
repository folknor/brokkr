//! The bench store: every attempted script-bench run, with its iterations,
//! their children and each child's full sidecar evidence, in one SQLite file.
//!
//! Self-contained on purpose: this module reaches only std, rusqlite, serde,
//! serde_json and xxhash-rust, never the rest of the brokkr crate (a textlint
//! rule holds it to that), so a second binary can include it by path, as the
//! rustc guard includes the lock probe, and put a network layer in front of
//! the same document, validation, migration chain and transaction. Path
//! resolution, directory creation and credential redaction stay with the
//! caller.
//!
//! Two write kinds, each in one `BEGIN IMMEDIATE` transaction whose request
//! id is a client uuid: a uuid already stored is answered from what was
//! stored when the requests are equal and refused when they differ, and the
//! lookup, comparison and insert share the transaction. [`Store::publish`]
//! takes a whole [`RunDocument`], its run uuid the request id;
//! [`Store::mark`] appends a [`MarkRequest`] (a retirement, invalidation or
//! rename, or an undo of one) beside the runs, never editing one. Readers open the store
//! without creating it ([`Store::open_read_only`]) and list runs through
//! [`RunFilter`]. A results server answers the same calls over HTTP through
//! [`wire`]. `reference/bench-store.md` is the contract.

mod document;
mod error;
mod list;
mod marks;
mod read;
mod schema;
pub mod wire;
mod write;

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

pub use document::{
    BoundParam, Build, CACHE_PROCEDURE, CacheFile, CacheMode, CachePreparation, CaptureOutcome,
    Child, ChildEnding, Clock, Conditions, DOCUMENT_FORMAT, Evidence, Executable,
    ExecutableIdentity, HotpathCapture, Iteration, IterationOutcome, Loss, Measurement, ParamValue,
    Record, ResolvedBuild, RunDocument, RunOutcome, STDERR_RUN_BYTES, STDERR_TAIL_BYTES, Sample,
    SinkEnd, StderrCapture, StderrTail,
};
pub use error::StoreError;
pub use list::{RunFilter, Selection, StoredRun};
pub use marks::{Mark, MarkOrigin, MarkRequest, MarkView, Marked, Revision};

/// What a publication did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Published {
    /// The run is new and now stored.
    Stored,
    /// The run was already stored with this exact content: a retry of a
    /// write that landed.
    AlreadyStored,
}

/// An open bench store.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open or create the store at `path`; its directory must exist. An older
    /// schema is migrated; a newer one, a foreign file or a filesystem
    /// without WAL is refused.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            conn: schema::open(path)?,
        })
    }

    /// Open an existing store for writing without creating it: `None` when
    /// there is no file at `path`. Migrated and refused as by [`Store::open`].
    pub fn open_existing(path: &Path) -> Result<Option<Self>, StoreError> {
        Ok(schema::open_existing(path)?.map(|conn| Self { conn }))
    }

    /// Publish one attempted run, whole or not at all.
    pub fn publish(&mut self, doc: &RunDocument) -> Result<Published, StoreError> {
        doc.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<i64> = tx
            .query_row("SELECT id FROM runs WHERE uuid = ?1", [&doc.uuid], |row| {
                row.get(0)
            })
            .optional()?;
        if existing.is_some() {
            let stored = read::load(&tx, &doc.uuid)?
                .ok_or_else(|| StoreError::Failed("a stored run vanished mid-read".to_owned()))?;
            // Dropping the transaction rolls back a read-only transaction.
            return if &stored == doc {
                Ok(Published::AlreadyStored)
            } else {
                Err(StoreError::Conflict {
                    uuid: doc.uuid.clone(),
                })
            };
        }
        write::insert(&tx, doc)?;
        tx.commit()
            .map_err(|e| StoreError::CommitUncertain(e.to_string()))?;
        Ok(Published::Stored)
    }

    /// Open an existing store for reading without creating it: `None` when
    /// there is no file at `path`. An older schema is migrated first; a
    /// newer one or a foreign file is refused, as by [`Store::open`].
    pub fn open_read_only(path: &Path) -> Result<Option<Self>, StoreError> {
        Ok(schema::open_read_only(path)?.map(|conn| Self { conn }))
    }

    /// The run stored under `uuid`, as it was published.
    pub fn load(&self, uuid: &str) -> Result<Option<RunDocument>, StoreError> {
        read::load(&self.conn, uuid)
    }

    /// The run stored under `uuid` with what the store derived from it,
    /// its children read only when `children` is set.
    pub fn run(&self, uuid: &str, children: bool) -> Result<Option<StoredRun>, StoreError> {
        read::load_run(&self.conn, uuid, children)
    }

    /// The uuids `filter` selects, newest first by start time, then uuid, at
    /// revision `at` (the current one when `None`), with the revision and the
    /// project's marks in effect at it.
    pub fn select(
        &self,
        filter: &RunFilter,
        at: Option<Revision>,
    ) -> Result<Selection, StoreError> {
        list::select(&self.conn, filter, at)
    }

    /// Append one mark request, or answer its replay.
    pub fn mark(&mut self, request: &MarkRequest) -> Result<Marked, StoreError> {
        marks::append(&mut self.conn, request)
    }

    /// Every mark request of `project`, in the order they applied.
    pub fn marks(&self, project: &str) -> Result<Vec<MarkRequest>, StoreError> {
        marks::log(&self.conn, project, i64::MAX)
    }

    /// Whether `project` has any stored run, whatever its outcome.
    pub fn has_runs(&self, project: &str) -> Result<bool, StoreError> {
        list::has_runs(&self.conn, project)
    }

    /// Runs of other projects whose uuid starts with `prefix`, as
    /// `(uuid, project)`.
    pub fn elsewhere(
        &self,
        project: &str,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        list::elsewhere(&self.conn, project, prefix)
    }

    /// Whether the connection is inside a transaction. A long-lived owner
    /// checks this after every operation: one left open by a failed
    /// rollback would poison every later request.
    pub fn in_transaction(&self) -> bool {
        !self.conn.is_autocommit()
    }

    /// The connection, for tests that inspect the schema itself.
    #[cfg(test)]
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// Each shipped migration step's xxh3-128, in chain order. A step's text
/// never changes once shipped: a schema change is a new step, and its hash
/// is appended here.
#[cfg(test)]
const SHIPPED_STEPS: &[&str] = &[
    "b9c408fb4eb3abd989cb949079736110",
    "b55bb417cdff92577ede441700a98333",
    "03cc48a74e55e5047728ea5f3bf15e06",
    "ec1a827868cc9bd5b45b3e34d43f397d",
    "bddf3dd75a32b844ae1d0325e4e864fd",
    "7b89417927c1c50bf88fd689361526d5",
    "daddc35349945664a22f0326bfbdecf2",
    "005a3850e0bf5c516cd15163877bd529",
];

#[cfg(test)]
mod frozen {
    #[test]
    fn shipped_migration_steps_never_change() {
        let actual: Vec<String> = super::schema::MIGRATIONS
            .iter()
            .map(|step| format!("{:032x}", xxhash_rust::xxh3::xxh3_128(step.as_bytes())))
            .collect();
        assert_eq!(
            actual,
            super::SHIPPED_STEPS,
            "a shipped step changed or a new one was not recorded: change the schema by appending a step"
        );
    }
}
