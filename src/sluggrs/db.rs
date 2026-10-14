//! Database adapter for sluggrs' visual snapshot tables.

use std::path::Path;

use crate::error::DevError;
use crate::visual::db::{self, DbSubject, VisualDb};
use crate::visual::status::Status;

pub(crate) type Approval = db::Approval;
pub(crate) type RunSummary = db::RunSummary;
pub(crate) type ResultRow = db::ResultRow;

pub(crate) struct SnapshotDb {
    inner: VisualDb,
}

impl SnapshotDb {
    /// Opens `results.db`, shared with `ResultsDb` and litehtml. The visual
    /// tables do not read or write `PRAGMA user_version`.
    pub fn open(path: &Path) -> Result<Self, DevError> {
        Ok(Self {
            inner: VisualDb::open(path, DbSubject::Sluggrs)?,
        })
    }

    pub fn insert_run(
        &self,
        run_id: &str,
        commit: &str,
        dirty: bool,
        adapter_name: Option<&str>,
    ) -> Result<(), DevError> {
        self.inner.insert_run(run_id, commit, dirty, adapter_name)
    }

    pub fn insert_result(
        &self,
        run_id: &str,
        snapshot_id: &str,
        pixel_diff_pct: Option<f64>,
        status: Status,
    ) -> Result<(), DevError> {
        self.inner
            .insert_result(run_id, snapshot_id, pixel_diff_pct, None, status)
    }

    pub fn get_approval(&self, snapshot_id: &str) -> Result<Option<Approval>, DevError> {
        self.inner.get_approval(snapshot_id)
    }

    pub fn set_approval(
        &self,
        snapshot_id: &str,
        commit: &str,
        pixel_diff_pct: f64,
    ) -> Result<(), DevError> {
        self.inner
            .set_approval(snapshot_id, commit, pixel_diff_pct, None)
    }

    #[allow(dead_code)]
    pub fn latest_run(&self) -> Result<Option<RunSummary>, DevError> {
        self.inner.latest_run()
    }

    pub fn run_results(&self, run_id: &str) -> Result<Vec<ResultRow>, DevError> {
        self.inner.run_results(run_id)
    }

    pub fn run_summary(&self, run_id: &str) -> Result<Option<RunSummary>, DevError> {
        self.inner.run_summary(run_id)
    }

    pub fn all_approvals(&self) -> Result<Vec<Approval>, DevError> {
        self.inner.all_approvals()
    }

    pub fn update_run_adapter(&self, run_id: &str, adapter_name: &str) -> Result<(), DevError> {
        self.inner.update_run_adapter(run_id, adapter_name)
    }

    pub fn latest_result_for_snapshot(
        &self,
        snapshot_id: &str,
    ) -> Result<Option<ResultRow>, DevError> {
        self.inner.latest_result_for_subject(snapshot_id)
    }
}
