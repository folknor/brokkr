//! Shared identity and selection policy for the results and sidecar stores.

use std::collections::BTreeSet;
use std::path::Path;

use super::{QueryFilter, ResultsDb, StoredRow, sidecar};
use crate::error::DevError;

/// One canonical UUID shared by the results and sidecar stores.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RunId(String);

impl RunId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn new(uuid: String) -> Self {
        Self(uuid)
    }
}

/// A user-supplied run reference. Resolution always happens through
/// [`RunStore`], which considers both stores and latest-key aliases.
#[derive(Clone, Copy)]
pub(crate) enum RunRef<'a> {
    Uuid(&'a str),
}

/// A selector that may name one or several runs.
#[derive(Clone, Copy)]
pub(crate) enum RunSelector<'a> {
    Uuid(&'a str),
    Commit(&'a str),
}

/// The shared result of resolving a user-supplied run reference.
pub(crate) enum Resolved {
    One(RunId),
    Ambiguous(Vec<RunId>),
    Missing,
}

/// Counts from deleting one exact run identity in both stores.
pub(crate) struct DeleteCounts {
    pub(crate) results_rows: usize,
    pub(crate) sidecar_sessions: usize,
}

/// Facade over the durable results database and the local sidecar database.
/// UUID resolution and selection are centralized here so each command can
/// apply its own policy to `One`, `Ambiguous`, and `Missing`.
pub(crate) struct RunStore {
    results: Option<ResultsDb>,
    sidecar: Option<sidecar::SidecarDb>,
}

impl RunStore {
    /// Open each existing store. Missing files remain absent rather than
    /// being created just to answer a query.
    pub(crate) fn open(results_path: &Path, sidecar_path: &Path) -> Result<Self, DevError> {
        let results = results_path
            .exists()
            .then(|| ResultsDb::open(results_path))
            .transpose()?;
        let sidecar = sidecar_path
            .exists()
            .then(|| sidecar::SidecarDb::open(sidecar_path))
            .transpose()?;
        Ok(Self { results, sidecar })
    }

    pub(crate) fn has_results_db(&self) -> bool {
        self.results.is_some()
    }

    pub(crate) fn has_sidecar_db(&self) -> bool {
        self.sidecar.is_some()
    }

    /// The formatter interface still consumes the database handle directly;
    /// command selection and all run identity decisions stay in this facade.
    pub(crate) fn sidecar_db(&self) -> Option<&sidecar::SidecarDb> {
        self.sidecar.as_ref()
    }

    pub(crate) fn results_db(&self) -> Option<&ResultsDb> {
        self.results.as_ref()
    }

    pub(crate) fn query_sidecar_meta(&self, id: &RunId) -> Result<(usize, usize), DevError> {
        self.sidecar_required()?.query_meta_for(id)
    }

    pub(crate) fn query_sidecar_samples(
        &self,
        id: &RunId,
        run_idx: Option<usize>,
    ) -> Result<Vec<crate::sidecar::Sample>, DevError> {
        self.sidecar_required()?.query_samples_for(id, run_idx)
    }

    pub(crate) fn query_sidecar_markers(
        &self,
        id: &RunId,
        run_idx: Option<usize>,
    ) -> Result<Vec<crate::sidecar::Marker>, DevError> {
        self.sidecar_required()?.query_markers_for(id, run_idx)
    }

    pub(crate) fn query_sidecar_counters(
        &self,
        id: &RunId,
        run_idx: Option<usize>,
    ) -> Result<Vec<crate::sidecar::Counter>, DevError> {
        self.sidecar_required()?.query_counters_for(id, run_idx)
    }

    /// Resolve a UUID or latest-key alias against the union of both stores.
    /// Candidate UUIDs are deduplicated when a run appears in both files.
    pub(crate) fn resolve(&self, run_ref: RunRef<'_>) -> Result<Resolved, DevError> {
        match run_ref {
            RunRef::Uuid(prefix) => {
                super::like::require_prefix(prefix, "uuid")?;
                let prefix = self.resolve_latest(prefix);
                super::like::require_prefix(&prefix, "uuid")?;
                let candidates = self.uuids_matching_prefix(&prefix)?;
                let resolution = match candidates.len() {
                    0 => Resolved::Missing,
                    1 => Resolved::One(candidates[0].clone()),
                    _ => Resolved::Ambiguous(candidates),
                };
                Ok(resolution)
            }
        }
    }

    /// Select every run matching a UUID or commit prefix across both stores.
    /// This is intentionally uncapped: callers that delete or audit a set of
    /// runs must never operate on a silently truncated selection.
    pub(crate) fn select_runs(&self, selector: RunSelector<'_>) -> Result<Vec<RunId>, DevError> {
        let candidates = match selector {
            RunSelector::Uuid(prefix) => {
                super::like::require_prefix(prefix, "uuid")?;
                let prefix = self.resolve_latest(prefix);
                super::like::require_prefix(&prefix, "uuid")?;
                self.uuids_matching_prefix(&prefix)?
            }
            RunSelector::Commit(prefix) => {
                super::like::require_prefix(prefix, "commit")?;
                let mut ids = BTreeSet::new();
                if let Some(results) = &self.results {
                    ids.extend(results.uuids_matching_commit_prefix(prefix)?);
                }
                if let Some(sidecar) = &self.sidecar {
                    ids.extend(sidecar.uuids_matching_commit_prefix(prefix)?);
                }
                ids.into_iter().map(RunId::new).collect()
            }
        };
        Ok(candidates)
    }

    /// Query rows for already selected identities. Returns the omitted row
    /// count after applying `filter.limit`, so UUID views can disclose the cap.
    pub(crate) fn query_results(
        &self,
        ids: &[RunId],
        filter: &QueryFilter,
    ) -> Result<(Vec<StoredRow>, usize), DevError> {
        match &self.results {
            Some(results) => results.query_by_run_ids(ids, filter),
            None => Ok((Vec::new(), 0)),
        }
    }

    pub(crate) fn has_result(&self, id: &RunId) -> Result<bool, DevError> {
        match &self.results {
            Some(results) => results.has_uuid(id.as_str()),
            None => Ok(false),
        }
    }

    pub(crate) fn result_preview(&self, id: &RunId) -> Result<Option<StoredRow>, DevError> {
        let Some(results) = &self.results else {
            return Ok(None);
        };
        let filter = QueryFilter {
            limit: 1,
            ..QueryFilter::default()
        };
        let (mut rows, _) = results.query_by_run_ids(std::slice::from_ref(id), &filter)?;
        Ok(rows.pop())
    }

    pub(crate) fn has_sidecar_data(&self, id: &RunId) -> Result<bool, DevError> {
        match &self.sidecar {
            Some(sidecar) => sidecar.has_data_for(id),
            None => Ok(false),
        }
    }

    /// Delete one canonical run identity from both stores.
    pub(crate) fn delete(&self, id: &RunId) -> Result<DeleteCounts, DevError> {
        let results_rows = match &self.results {
            Some(results) => results.delete_by_uuid(id)?,
            None => 0,
        };
        let sidecar_sessions = match &self.sidecar {
            Some(sidecar) => sidecar.delete_by_run_id(id)?,
            None => 0,
        };
        Ok(DeleteCounts {
            results_rows,
            sidecar_sessions,
        })
    }

    fn resolve_latest(&self, value: &str) -> String {
        self.sidecar
            .as_ref()
            .map(|sidecar| sidecar.resolve_latest(value))
            .unwrap_or_else(|| value.to_owned())
    }

    fn sidecar_required(&self) -> Result<&sidecar::SidecarDb, DevError> {
        self.sidecar
            .as_ref()
            .ok_or_else(|| DevError::Config("sidecar database is not available".into()))
    }

    fn uuids_matching_prefix(&self, prefix: &str) -> Result<Vec<RunId>, DevError> {
        let mut ids = BTreeSet::new();
        if let Some(results) = &self.results {
            ids.extend(results.uuids_matching_prefix(prefix)?);
        }
        if let Some(sidecar) = &self.sidecar {
            ids.extend(sidecar.uuids_matching_prefix(prefix)?);
        }
        Ok(ids.into_iter().map(RunId::new).collect())
    }
}
