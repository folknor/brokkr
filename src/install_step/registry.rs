//! The ownership registry: one JSON record per copy destination under
//! `~/.brokkr/install/`, holding who installed what is there and the journal
//! of an install in flight.
//!
//! A record's file is named by a hash of its destination, and the destination
//! is stored inside it and compared on every read, so a name collision can
//! never hand one destination's authority to another. Every write is a
//! replace-and-fsync followed by a strict fsync of the registry directory: a
//! phase is only acted on once its record is durable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::tree;
use crate::error::DevError;

const VERSION: u32 = 1;

/// Who published the object at a destination, and its fingerprint then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Owner {
    pub project: String,
    pub step: String,
    pub fingerprint: String,
}

/// A directory's identity, recorded for the private container only. It
/// authenticates a directory brokkr created exclusively under a random name;
/// it is never ownership evidence for a published artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct DirId {
    pub dev: u64,
    pub ino: u64,
}

/// The journal of an install in flight. Each phase carries what its outcome
/// needs; `prior` is always the record's owner before the transaction, which
/// an abort restores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
pub(super) enum Pending {
    /// Written before the container is created.
    Allocated { container: String, prior: Option<Owner> },
    /// The container exists, is durable and has this identity; the payload
    /// is being copied into it.
    Preparing { container: String, id: DirId, prior: Option<Owner> },
    /// The payload is complete with `staged`'s fingerprint; `displaced` is
    /// the fingerprint of the destination publication will displace (`None`
    /// for an absent destination).
    Prepared {
        container: String,
        id: DirId,
        prior: Option<Owner>,
        staged: Owner,
        displaced: Option<String>,
    },
    /// Published and verified; the container is being removed, then `owner`
    /// is committed.
    Retiring { container: String, id: DirId, owner: Owner },
    /// Not published; the container is being removed, then `prior` is
    /// restored.
    Aborting { container: String, id: DirId, prior: Option<Owner> },
}

impl Pending {
    pub(super) fn container(&self) -> &str {
        match self {
            Self::Allocated { container, .. }
            | Self::Preparing { container, .. }
            | Self::Prepared { container, .. }
            | Self::Retiring { container, .. }
            | Self::Aborting { container, .. } => container,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Record {
    pub version: u32,
    pub destination: String,
    pub owner: Option<Owner>,
    pub pending: Option<Pending>,
    /// The identity of the destination's parent directory the transaction
    /// runs in; set whenever `pending` is. Recovery acts only when the
    /// parent path still names it, since a container missing from a
    /// replaced parent is not gone, only elsewhere.
    pub parent: Option<DirId>,
}

impl Record {
    pub(super) fn new(destination: &str, owner: Option<Owner>) -> Self {
        Self { version: VERSION, destination: destination.to_owned(), owner, pending: None, parent: None }
    }

    /// End the transaction with `owner` holding the destination.
    pub(super) fn settle(&mut self, owner: Option<Owner>) {
        self.owner = owner;
        self.pending = None;
        self.parent = None;
    }
}

pub(super) struct Registry {
    dir: PathBuf,
}

/// What [`Registry::all`] found.
#[derive(Default)]
pub(super) struct Scan {
    pub records: Vec<(PathBuf, Record)>,
    /// One message per record file that could not be trusted.
    pub broken: Vec<String>,
}

impl Registry {
    /// The registry kept in `dir` ([`crate::user_dirs::install_registry`]
    /// in production).
    pub(super) fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub(super) fn file_for(&self, destination: &str) -> PathBuf {
        let digest = tree::sha256_hex(destination.as_bytes());
        self.dir.join(format!("{}.json", &digest[..32]))
    }

    /// The record for `destination`, if one exists.
    pub(super) fn load(&self, destination: &str) -> Result<Option<Record>, DevError> {
        let path = self.file_for(destination);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(registry_error(&path, &e.to_string())),
        };
        let record = parse(&path, &text)?;
        if record.destination != destination {
            return Err(registry_error(
                &path,
                &format!("records {:?}, not {destination:?}", record.destination),
            ));
        }
        Ok(Some(record))
    }

    /// Every record, with its file, and every record file that cannot be
    /// trusted - unreadable, unparseable, another version's, or misnamed for
    /// its destination. Those are reported rather than fatal, so one bad file
    /// does not stop every install on the machine; a caller that needs the
    /// whole picture (a copy step's nesting check) refuses on them instead.
    pub(super) fn all(&self) -> Result<Scan, DevError> {
        let mut scan = Scan::default();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(scan),
            Err(e) => return Err(registry_error(&self.dir, &e.to_string())),
        };
        for entry in entries {
            let path = entry.map_err(|e| registry_error(&self.dir, &e.to_string()))?.path();
            let is_record = path.extension().is_some_and(|x| x == "json")
                && !path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'));
            if !is_record {
                continue;
            }
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| registry_error(&path, &e.to_string()))
                .and_then(|text| parse(&path, &text));
            match parsed {
                Ok(record) if self.file_for(&record.destination) == path => scan.records.push((path, record)),
                Ok(record) => scan.broken.push(registry_error(
                    &path,
                    &format!("records {:?}, whose record file has another name", record.destination),
                )
                .to_string()),
                Err(e) => scan.broken.push(e.to_string()),
            }
        }
        scan.records.sort_by(|a, b| a.1.destination.cmp(&b.1.destination));
        Ok(scan)
    }

    /// Make `record` durable. A record with neither owner nor transaction
    /// says nothing, so its file is removed instead.
    pub(super) fn store(&self, record: &Record) -> Result<(), DevError> {
        let path = self.file_for(&record.destination);
        if record.owner.is_none() && record.pending.is_none() {
            return match std::fs::remove_file(&path) {
                Ok(()) => tree::fsync_dir(&self.dir).map_err(|e| registry_error(&self.dir, &e.to_string())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(registry_error(&path, &e.to_string())),
            };
        }
        if !self.dir.is_dir() {
            std::fs::create_dir_all(&self.dir).map_err(|e| registry_error(&self.dir, &e.to_string()))?;
            if let Some(parent) = self.dir.parent() {
                tree::fsync_dir(parent).map_err(|e| registry_error(parent, &e.to_string()))?;
            }
        }
        let json = serde_json::to_vec_pretty(record)
            .map_err(|e| DevError::Internal(format!("install record does not serialize: {e}")))?;
        crate::atomic_write::replace(&path, &json).map_err(|e| registry_error(&path, &e.to_string()))?;
        tree::fsync_dir(&self.dir).map_err(|e| registry_error(&self.dir, &e.to_string()))
    }
}

fn parse(path: &Path, text: &str) -> Result<Record, DevError> {
    let record: Record = serde_json::from_str(text).map_err(|e| registry_error(path, &e.to_string()))?;
    if record.version != VERSION {
        return Err(registry_error(
            path,
            &format!("record version {} is not {VERSION}, which this brokkr reads", record.version),
        ));
    }
    Ok(record)
}

fn registry_error(path: &Path, msg: &str) -> DevError {
    DevError::Refused(format!("install registry {}: {msg}", path.display()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_record_round_trips_and_an_empty_one_is_removed() {
        let state = crate::test_scratch::scratch("install_step_registry", "round_trip");
        let registry = Registry::at(state.join("install"));
        let owner = Owner { project: "p".into(), step: "s".into(), fingerprint: "ab".into() };
        let mut record = Record::new("/x/y", Some(owner.clone()));
        record.pending = Some(Pending::Allocated { container: ".brokkr-install-1".into(), prior: Some(owner) });
        registry.store(&record).unwrap();
        assert_eq!(registry.load("/x/y").unwrap(), Some(record.clone()));
        assert_eq!(registry.all().unwrap().records.len(), 1);

        record.settle(None);
        registry.store(&record).unwrap();
        assert_eq!(registry.load("/x/y").unwrap(), None);
        assert!(registry.all().unwrap().records.is_empty());
    }

    #[test]
    fn a_record_for_another_destination_is_refused() {
        let state = crate::test_scratch::scratch("install_step_registry", "mismatch");
        let registry = Registry::at(state.join("install"));
        registry.store(&Record::new("/a", Some(Owner {
            project: "p".into(),
            step: "s".into(),
            fingerprint: "f".into(),
        })))
        .unwrap();
        std::fs::rename(registry.file_for("/a"), registry.file_for("/b")).unwrap();
        assert!(registry.load("/b").is_err());
        let scan = registry.all().unwrap();
        assert!(scan.records.is_empty(), "enumeration checks the file name too");
        assert_eq!(scan.broken.len(), 1);
        std::fs::write(registry.file_for("/c"), "not json").unwrap();
        assert_eq!(registry.all().unwrap().broken.len(), 2, "an unparseable file is reported, not fatal");
    }
}
