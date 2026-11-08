//! Marks: what a user said about stored runs after the fact, appended beside
//! them and never written into them. A bench name is retired or renamed, a
//! run invalidated, and each of those undone, by appending one request to
//! the `marks` log; no stored run is edited or deleted.
//!
//! Each request carries a client-generated uuid that is its request id, as a
//! run's uuid is its publication's: a replay equal to the stored request is
//! [`Marked::AlreadyApplied`], an unequal one under that uuid a conflict, and
//! that lookup comes before any check against the current state, so a
//! request whose effect a later mark undid still answers its replay.
//!
//! What the marks mean is one fold over a project's log in apply order
//! ([`MarkView`]). Admission checks a proposed mark against that fold and
//! the runs table inside the write transaction; the fold itself applies
//! every accepted mark unconditionally and never consults runs, so what a
//! mark did at admission is what it does on every later read.
//!
//! The rules:
//!
//! - Renames form chains resolved to their end. A name is the source of at
//!   most one rename. Renaming onto a name the store knows (one with runs,
//!   or that a rename or retirement names) is refused, except a rename onto
//!   an ancestor in the source's own chain: that is the undo, and it cancels
//!   every rename on the path, so no cycle is ever held. The no-merge check
//!   sees the runs stored when the rename commits; a run of the new name
//!   published later joins the renamed history.
//! - Retirement belongs to an end name, and a rename never changes any run's
//!   visibility: a forward rename moves the retirement to the new end, and
//!   an undo copies it to every name on the cancelled path. Retiring or
//!   unretiring a renamed name acts on its end. A run published later under
//!   a retired name is retired too.
//! - A run is hidden when it is invalidated or its resolved name is retired.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use super::StoreError;
use super::document::is_lower_hex;

/// One mark request, as submitted and as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkRequest {
    /// Client-generated, 32 lowercase hex digits: the request id.
    pub uuid: String,
    pub project: String,
    /// When the client made the request.
    pub at_unix_ms: i64,
    /// The host the client ran on.
    pub hostname: String,
    pub mark: Mark,
}

/// What a mark says. Externally tagged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Mark {
    /// Hide every run whose name resolves to `name`'s end.
    Retire { name: String, reason: String },
    /// Show them again.
    Unretire { name: String },
    /// Hide one run, by its full uuid.
    Invalidate { run: String, reason: String },
    /// Show it again.
    Revalidate { run: String },
    /// Runs named `from` (and every name renamed to it) now resolve to `to`.
    Rename { from: String, to: String },
}

impl Mark {
    fn kind(&self) -> &'static str {
        match self {
            Self::Retire { .. } => "retire",
            Self::Unretire { .. } => "unretire",
            Self::Invalidate { .. } => "invalidate",
            Self::Revalidate { .. } => "revalidate",
            Self::Rename { .. } => "rename",
        }
    }
}

/// What a mark request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Marked {
    /// The request is new and now stored.
    Applied,
    /// The request was already stored with this exact content: a retry of a
    /// write that landed.
    AlreadyApplied,
}

/// The request a retirement or invalidation in effect came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkOrigin {
    pub uuid: String,
    pub at_unix_ms: i64,
    pub hostname: String,
    pub reason: String,
}

/// A point in the store's history a read is pinned to: the highest mark row
/// and the highest run row it sees. Runs are inserted whole and never
/// deleted, and marks only appended, so the same revision always selects
/// the same runs and resolves them the same way. The two bounds are
/// separate because neither orders the other's rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub marks: i64,
    pub runs: i64,
}

/// One project's marks in effect at one revision. Readers resolve every run
/// they show against one view, so a listing or a comparison never mixes two.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkView {
    /// Every rename in effect, source to target.
    pub renames: BTreeMap<String, String>,
    /// Retired end names.
    pub retired: BTreeMap<String, MarkOrigin>,
    /// Invalidated runs by uuid.
    pub invalidated: BTreeMap<String, MarkOrigin>,
}

impl MarkView {
    /// The end of `name`'s rename chain; `name` itself when it is not
    /// renamed. The view never holds a cycle, but a chain is walked at most
    /// once around regardless.
    pub fn name<'a>(&'a self, name: &'a str) -> &'a str {
        let mut current = name;
        for _ in 0..=self.renames.len() {
            match self.renames.get(current) {
                Some(next) => current = next,
                None => break,
            }
        }
        current
    }

    /// The retirement a run named `bench` is under, if any.
    pub fn retired(&self, bench: &str) -> Option<&MarkOrigin> {
        self.retired.get(self.name(bench))
    }

    /// The invalidation of run `uuid`, if any.
    pub fn invalidated(&self, uuid: &str) -> Option<&MarkOrigin> {
        self.invalidated.get(uuid)
    }

    /// Whether a run is hidden: invalidated, or its name retired.
    pub fn hidden(&self, uuid: &str, bench: &str) -> bool {
        self.invalidated(uuid).is_some() || self.retired(bench).is_some()
    }

    /// A stored pairing key with its bench element replaced by the bench's
    /// resolved name, so runs of one bench across a rename pair. The tag and
    /// class stay: two keys of different algorithms never become equal. A
    /// key whose shape is not a known tag's comes back unchanged. The stored
    /// key itself is never rewritten.
    pub fn comparison_key(&self, pairing_key: &str) -> String {
        let Ok(serde_json::Value::Array(mut parts)) =
            serde_json::from_str::<serde_json::Value>(pairing_key)
        else {
            return pairing_key.to_owned();
        };
        let at = match parts.first().and_then(serde_json::Value::as_str) {
            Some("bench-pair-v1") => 2,
            Some("bench-pair-v2") => 3,
            _ => return pairing_key.to_owned(),
        };
        let Some(bench) = parts.get(at).and_then(serde_json::Value::as_str) else {
            return pairing_key.to_owned();
        };
        let resolved = self.name(bench).to_owned();
        parts[at] = serde_json::Value::String(resolved);
        serde_json::Value::Array(parts).to_string()
    }

    /// The names the chain from `name` passes through, `name` first and its
    /// end last.
    fn path(&self, name: &str) -> Vec<String> {
        let mut path = vec![name.to_owned()];
        for _ in 0..self.renames.len() {
            match self.renames.get(path.last().map_or("", String::as_str)) {
                Some(next) => path.push(next.clone()),
                None => break,
            }
        }
        path
    }

    /// Whether some rename names `name` as its target.
    fn is_target(&self, name: &str) -> bool {
        self.renames.values().any(|target| target == name)
    }

    /// Apply one accepted mark. Never refuses and never consults runs:
    /// admission already decided, and the fold must reproduce what it did.
    fn apply(&mut self, request: &MarkRequest) {
        let origin = |reason: &str| MarkOrigin {
            uuid: request.uuid.clone(),
            at_unix_ms: request.at_unix_ms,
            hostname: request.hostname.clone(),
            reason: reason.to_owned(),
        };
        match &request.mark {
            Mark::Retire { name, reason } => {
                let end = self.name(name).to_owned();
                self.retired.insert(end, origin(reason));
            }
            Mark::Unretire { name } => {
                let end = self.name(name).to_owned();
                self.retired.remove(&end);
            }
            Mark::Invalidate { run, reason } => {
                self.invalidated.insert(run.clone(), origin(reason));
            }
            Mark::Revalidate { run } => {
                self.invalidated.remove(run);
            }
            Mark::Rename { from, to } => {
                let path = self.path(to);
                if path.last() == Some(from) {
                    // The undo: `to` is an ancestor of `from`. Every name on
                    // the path becomes an end again, each carrying the
                    // retirement the old end had.
                    let carried = self.retired.get(from).cloned();
                    for name in &path {
                        self.renames.remove(name);
                        if let Some(origin) = &carried {
                            self.retired.insert(name.clone(), origin.clone());
                        }
                    }
                } else {
                    self.renames.insert(from.clone(), to.clone());
                    if let Some(origin) = self.retired.remove(from) {
                        self.retired.insert(to.clone(), origin);
                    }
                }
            }
        }
    }
}

fn refuse(message: impl Into<String>) -> StoreError {
    StoreError::Refused(message.into())
}

impl MarkRequest {
    /// Refuse a request no state could accept: malformed identity or an
    /// empty name, run or reason.
    pub fn validate(&self) -> Result<(), StoreError> {
        if !is_lower_hex(&self.uuid, &[32]) {
            return Err(refuse(format!(
                "mark uuid {:?} is not 32 lowercase hex digits",
                self.uuid
            )));
        }
        for (field, value) in [("project", &self.project), ("hostname", &self.hostname)] {
            if value.is_empty() {
                return Err(refuse(format!("{field} is empty")));
            }
        }
        match &self.mark {
            Mark::Retire { name, reason } => {
                nonempty("name", name)?;
                reason_given(reason)
            }
            Mark::Unretire { name } => nonempty("name", name),
            Mark::Invalidate { run, reason } => {
                run_uuid(run)?;
                reason_given(reason)
            }
            Mark::Revalidate { run } => run_uuid(run),
            Mark::Rename { from, to } => {
                nonempty("from", from)?;
                nonempty("to", to)?;
                if from == to {
                    return Err(refuse(format!("{from} cannot be renamed to itself")));
                }
                Ok(())
            }
        }
    }
}

fn nonempty(field: &str, value: &str) -> Result<(), StoreError> {
    if value.is_empty() {
        return Err(refuse(format!("{field} is empty")));
    }
    Ok(())
}

fn reason_given(reason: &str) -> Result<(), StoreError> {
    if reason.trim().is_empty() {
        return Err(refuse("a reason is required"));
    }
    Ok(())
}

fn run_uuid(run: &str) -> Result<(), StoreError> {
    if !is_lower_hex(run, &[32]) {
        return Err(refuse(format!(
            "run {run:?} is not a full run uuid (32 lowercase hex digits)"
        )));
    }
    Ok(())
}

/// The store's current revision.
pub(super) fn current(conn: &Connection) -> Result<Revision, StoreError> {
    Ok(conn.query_row(
        "SELECT (SELECT COALESCE(MAX(id), 0) FROM marks),
                (SELECT COALESCE(MAX(id), 0) FROM runs)",
        [],
        |row| {
            Ok(Revision {
                marks: row.get(0)?,
                runs: row.get(1)?,
            })
        },
    )?)
}

/// `at` when given, refused when past the store's current revision (a pin
/// that would admit rows written later is no pin); else the current one.
pub(super) fn pinned(conn: &Connection, at: Option<Revision>) -> Result<Revision, StoreError> {
    let now = current(conn)?;
    let Some(at) = at else {
        return Ok(now);
    };
    if at.marks < 0 || at.runs < 0 || at.marks > now.marks || at.runs > now.runs {
        return Err(refuse(format!(
            "revision marks {} runs {} is not one this store has reached (marks {} runs {})",
            at.marks, at.runs, now.marks, now.runs
        )));
    }
    Ok(at)
}

/// Every mark request of `project` up to mark row `through`, in apply order.
pub(super) fn log(
    conn: &Connection,
    project: &str,
    through: i64,
) -> Result<Vec<MarkRequest>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT uuid, at_unix_ms, hostname, kind, name, target, run, reason FROM marks
         WHERE project = ?1 AND id <= ?2 ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![project, through], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(
            |(uuid, at_unix_ms, hostname, kind, name, target, run, reason)| {
                let mark = match (kind.as_str(), name, target, run, reason) {
                    ("retire", Some(name), None, None, Some(reason)) => {
                        Mark::Retire { name, reason }
                    }
                    ("unretire", Some(name), None, None, None) => Mark::Unretire { name },
                    ("invalidate", None, None, Some(run), Some(reason)) => {
                        Mark::Invalidate { run, reason }
                    }
                    ("revalidate", None, None, Some(run), None) => Mark::Revalidate { run },
                    ("rename", Some(from), Some(to), None, None) => Mark::Rename { from, to },
                    _ => return Err(StoreError::Failed("stored mark does not read back".into())),
                };
                Ok(MarkRequest {
                    uuid,
                    project: project.to_owned(),
                    at_unix_ms,
                    hostname,
                    mark,
                })
            },
        )
        .collect()
}

/// `project`'s marks in effect at mark row `through`.
pub(super) fn view(conn: &Connection, project: &str, through: i64) -> Result<MarkView, StoreError> {
    let mut view = MarkView::default();
    for request in log(conn, project, through)? {
        view.apply(&request);
    }
    Ok(view)
}

/// The stored request under `uuid`, with its project.
fn stored(conn: &Connection, uuid: &str) -> Result<Option<MarkRequest>, StoreError> {
    let found: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, project FROM marks WHERE uuid = ?1",
            [uuid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((id, project)) = found else {
        return Ok(None);
    };
    let request = log(conn, &project, id)?
        .pop()
        .filter(|request| request.uuid == uuid)
        .ok_or_else(|| StoreError::Failed("a stored mark vanished mid-read".to_owned()))?;
    Ok(Some(request))
}

/// Whether `project` has a run literally named `bench`.
fn has_bench(conn: &Connection, project: &str, bench: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE project = ?1 AND bench = ?2)",
        [project, bench],
        |row| row.get(0),
    )?)
}

/// Whether the store knows `name` in this project: runs under it, or a mark
/// in effect that names it.
fn known(
    conn: &Connection,
    view: &MarkView,
    project: &str,
    name: &str,
) -> Result<bool, StoreError> {
    Ok(view.renames.contains_key(name)
        || view.is_target(name)
        || view.retired.contains_key(name)
        || has_bench(conn, project, name)?)
}

/// Admission: refuse a mark the current state cannot take.
fn admit(conn: &Connection, view: &MarkView, request: &MarkRequest) -> Result<(), StoreError> {
    let project = request.project.as_str();
    let unknown = |name: &str| refuse(format!("no bench named {name} in project {project}"));
    match &request.mark {
        Mark::Retire { name, .. } => {
            let end = view.name(name);
            if !known(conn, view, project, name)? {
                return Err(unknown(name));
            }
            if let Some(origin) = view.retired.get(end) {
                return Err(refuse(format!(
                    "{end} is already retired: {}",
                    origin.reason
                )));
            }
        }
        Mark::Unretire { name } => {
            let end = view.name(name);
            if !view.retired.contains_key(end) {
                return Err(refuse(format!("{end} is not retired")));
            }
        }
        Mark::Invalidate { run, .. } => {
            let exists: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM runs WHERE project = ?1 AND uuid = ?2)",
                [project, run],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(refuse(format!("no run {run} in project {project}")));
            }
            if let Some(origin) = view.invalidated.get(run) {
                return Err(refuse(format!(
                    "run {run} is already invalidated: {}",
                    origin.reason
                )));
            }
        }
        Mark::Revalidate { run } => {
            if !view.invalidated.contains_key(run) {
                return Err(refuse(format!("run {run} is not invalidated")));
            }
        }
        Mark::Rename { from, to } => {
            if let Some(next) = view.renames.get(from) {
                return Err(refuse(format!(
                    "{from} is already renamed to {next}; rename {} instead",
                    view.name(from)
                )));
            }
            if !known(conn, view, project, from)? {
                return Err(unknown(from));
            }
            let undo = view.path(to).last() == Some(from);
            if !undo && known(conn, view, project, to)? {
                return Err(refuse(format!(
                    "{to} already names runs or a mark in project {project}; renaming {from} onto it would merge them"
                )));
            }
        }
    }
    Ok(())
}

/// Store one mark request in its own `BEGIN IMMEDIATE`: answered from the
/// stored request when its uuid is already there, else admitted against the
/// current state and appended.
pub(super) fn append(conn: &mut Connection, request: &MarkRequest) -> Result<Marked, StoreError> {
    // Only the uuid is judged before the lookup: any request unequal to the
    // stored one under its uuid is a conflict, an invalid one included.
    if !is_lower_hex(&request.uuid, &[32]) {
        request.validate()?;
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(stored) = stored(&tx, &request.uuid)? {
        return if &stored == request {
            Ok(Marked::AlreadyApplied)
        } else {
            Err(StoreError::Conflict {
                uuid: request.uuid.clone(),
            })
        };
    }
    request.validate()?;
    let now = current(&tx)?;
    let view = view(&tx, &request.project, now.marks)?;
    admit(&tx, &view, request)?;
    insert(&tx, request)?;
    tx.commit()
        .map_err(|e| StoreError::CommitUncertain(e.to_string()))?;
    Ok(Marked::Applied)
}

fn insert(tx: &Transaction<'_>, request: &MarkRequest) -> Result<(), StoreError> {
    let (name, target, run, reason) = match &request.mark {
        Mark::Retire { name, reason } => (Some(name), None, None, Some(reason)),
        Mark::Unretire { name } => (Some(name), None, None, None),
        Mark::Invalidate { run, reason } => (None, None, Some(run), Some(reason)),
        Mark::Revalidate { run } => (None, None, Some(run), None),
        Mark::Rename { from, to } => (Some(from), Some(to), None, None),
    };
    tx.execute(
        "INSERT INTO marks (uuid, project, at_unix_ms, hostname, kind, name, target, run, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            request.uuid,
            request.project,
            request.at_unix_ms,
            request.hostname,
            request.mark.kind(),
            name,
            target,
            run,
            reason
        ],
    )?;
    Ok(())
}
