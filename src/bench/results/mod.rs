//! `brokkr results` over the bench store: the current project's script-bench
//! runs, listed, looked up by uuid and compared between two commits, beside
//! the typed rows of `results.db` (`docs/commands/results.md`, Script
//! benches). The store answers the queries (`store::RunFilter`); this module
//! maps `results`' filters onto them and renders what comes back.

mod compare;
mod config_diff;
mod single;
mod table;

pub(crate) use compare::format_compare;
pub(crate) use single::format_single;
pub(crate) use table::format_table;

use super::store::{self, RunFilter, StoredRun};
use crate::error::DevError;

/// `results`' row filters as they apply to bench runs.
#[derive(Debug, Clone, Default)]
pub(crate) struct BenchFilter {
    pub(crate) commit: Option<String>,
    /// `--command`: a substring of the bench name.
    pub(crate) bench: Option<String>,
    pub(crate) env: Vec<(String, String)>,
    /// `--param NAME=VALUE`.
    pub(crate) params: Vec<(String, String)>,
    pub(crate) grep: Vec<String>,
    pub(crate) grep_v: Vec<String>,
    /// A filter no bench run can answer (`--mode`, `--dataset`, `--meta`)
    /// was given, so every bench run is excluded.
    pub(crate) excluded: bool,
    pub(crate) limit: usize,
}

/// The per-user bench store, opened for reading on behalf of one project.
pub(crate) struct BenchReader {
    store: store::Store,
    project: String,
    /// The store's file, named by every error.
    path: std::path::PathBuf,
}

/// What a uuid prefix found among the project's bench runs.
pub(crate) struct Lookup {
    /// The matching runs within `--limit`, newest first; children loaded.
    pub(crate) runs: Vec<StoredRun>,
    /// Matching runs past `--limit`.
    pub(crate) omitted: usize,
    /// Every matching run before the filters, for the candidate count.
    pub(crate) candidates: usize,
    /// Runs of other projects the prefix names, as `(uuid, project)`.
    pub(crate) elsewhere: Vec<(String, String)>,
}

fn named(path: &std::path::Path, error: &store::StoreError) -> DevError {
    DevError::Database(format!("{}: {error}", path.display()))
}

fn require_prefix(value: &str, what: &str) -> Result<(), DevError> {
    if value.is_empty() {
        return Err(DevError::Usage(format!(
            "an empty {what} prefix would match every run; give at least one digit"
        )));
    }
    Ok(())
}

impl BenchReader {
    /// Open the bench store for `project`. `None` when there is no store: no
    /// file, or no data home to name one in. A store that exists but cannot
    /// be read (a newer schema, a foreign file) is an error.
    pub(crate) fn open(project: &str) -> Result<Option<Self>, DevError> {
        let Ok(path) = super::evidence::store_path() else {
            return Ok(None);
        };
        Self::at(&path, project)
    }

    /// [`Self::open`] on the store at `path`.
    pub(crate) fn at(path: &std::path::Path, project: &str) -> Result<Option<Self>, DevError> {
        let store = store::Store::open_read_only(path).map_err(|e| named(path, &e))?;
        Ok(store.map(|store| Self {
            store,
            project: project.to_owned(),
            path: path.to_owned(),
        }))
    }

    /// A store error naming the file, whether the open or a later query
    /// failed.
    fn named(&self, error: &store::StoreError) -> DevError {
        named(&self.path, error)
    }

    /// Whether the project has any bench run, whatever its outcome.
    pub(crate) fn has_runs(&self) -> Result<bool, DevError> {
        self.store
            .has_runs(&self.project)
            .map_err(|e| self.named(&e))
    }

    fn select(&self, filter: &RunFilter) -> Result<Vec<String>, DevError> {
        self.store.select(filter).map_err(|e| self.named(&e))
    }

    fn filter(&self, filter: &BenchFilter) -> RunFilter {
        RunFilter {
            project: self.project.clone(),
            commit_prefix: filter.commit.clone(),
            bench: filter.bench.clone(),
            env: filter.env.clone(),
            params: filter.params.clone(),
            grep: filter.grep.clone(),
            grep_v: filter.grep_v.clone(),
            ..RunFilter::default()
        }
    }

    fn load(&self, uuids: &[String], children: bool) -> Result<Vec<StoredRun>, DevError> {
        uuids
            .iter()
            .map(|uuid| {
                self.store
                    .run(uuid, children)
                    .map_err(|e| self.named(&e))?
                    .ok_or_else(|| {
                        DevError::Database(format!(
                            "{}: bench run {uuid} vanished while reading",
                            self.path.display()
                        ))
                    })
            })
            .collect()
    }

    /// The newest runs the filter selects, at most `filter.limit`.
    pub(crate) fn table(&self, filter: &BenchFilter) -> Result<Vec<StoredRun>, DevError> {
        if filter.excluded {
            return Ok(Vec::new());
        }
        if let Some(commit) = &filter.commit {
            require_prefix(commit, "commit")?;
        }
        let mut uuids = self.select(&self.filter(filter))?;
        uuids.truncate(filter.limit);
        self.load(&uuids, false)
    }

    /// The runs whose uuid starts with `prefix`, filtered and capped.
    pub(crate) fn lookup(&self, prefix: &str, filter: &BenchFilter) -> Result<Lookup, DevError> {
        require_prefix(prefix, "uuid")?;
        let unfiltered = RunFilter {
            project: self.project.clone(),
            uuid_prefix: Some(prefix.to_owned()),
            ..RunFilter::default()
        };
        let candidates = self.select(&unfiltered)?.len();
        let mut uuids = if filter.excluded {
            Vec::new()
        } else {
            self.select(&RunFilter {
                uuid_prefix: Some(prefix.to_owned()),
                ..self.filter(filter)
            })?
        };
        let omitted = uuids.len().saturating_sub(filter.limit);
        uuids.truncate(filter.limit);
        let elsewhere = if candidates == 0 {
            self.store
                .elsewhere(&self.project, prefix)
                .map_err(|e| self.named(&e))?
        } else {
            Vec::new()
        };
        Ok(Lookup {
            runs: self.load(&uuids, true)?,
            omitted,
            candidates,
            elsewhere,
        })
    }

    /// Each side of a comparison: the completed runs of a clean tree whose
    /// commit starts with that side's prefix and pass the filters, newest
    /// first and uncapped (pairs are formed before `--limit` applies).
    pub(crate) fn compare_sides(
        &self,
        commit_a: &str,
        commit_b: &str,
        filter: &BenchFilter,
    ) -> Result<(Vec<StoredRun>, Vec<StoredRun>), DevError> {
        require_prefix(commit_a, "commit")?;
        require_prefix(commit_b, "commit")?;
        if filter.excluded {
            return Ok((Vec::new(), Vec::new()));
        }
        let side = |commit: &str| -> Result<Vec<StoredRun>, DevError> {
            let uuids = self.select(&RunFilter {
                commit_prefix: Some(commit.to_owned()),
                with_result: true,
                clean: true,
                ..self.filter(filter)
            })?;
            self.load(&uuids, false)
        };
        Ok((side(commit_a)?, side(commit_b)?))
    }
}

/// A param's value as the table lists it and `--param` matches it: its keys
/// joined by `,` in bound order.
pub(crate) fn param_value(param: &store::BoundParam) -> String {
    param
        .values
        .iter()
        .map(|v| v.key.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Every bound param as `NAME=VALUE`, by name.
fn params_summary(run: &StoredRun) -> String {
    run.doc
        .params
        .iter()
        .map(|(name, param)| format!("{name}={}", param_value(param)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A verdict as every view shows it: nil, or the string quoted, so `""`
/// stays visible and distinct from nil.
fn verdict(verdict: Option<&String>) -> String {
    verdict.map_or_else(|| "nil".to_owned(), |v| format!("{v:?}"))
}

/// The result's verdict: `None` for a run without a result, `Some(None)` for
/// a nil one.
fn result_verdict(run: &StoredRun) -> Option<Option<&String>> {
    let best = run.doc.iterations.get(run.best_iteration?)?;
    match &best.outcome {
        store::IterationOutcome::Completed { verdict } => Some(verdict.as_ref()),
        _ => None,
    }
}

/// The result's wall in microseconds.
fn result_wall(run: &StoredRun) -> Option<i64> {
    run.doc.iterations.get(run.best_iteration?)?.wall_us
}

/// Microseconds as milliseconds with three decimals, the precision every
/// `results` view prints a measured wall at.
fn ms(us: i64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let ms = us as f64 / 1000.0;
    format!("{ms:.3} ms")
}

/// A commit as the views show it: eight hex digits, `-dirty` for a run of a
/// dirty tree.
fn short_commit(run: &StoredRun) -> String {
    let short: String = run.doc.commit.chars().take(8).collect();
    if run.doc.dirty {
        format!("{short}-dirty")
    } else {
        short
    }
}

/// A run's start as `YYYY-MM-DD HH:MM:SS` UTC, the form `results.db` rows
/// carry.
fn timestamp(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1000);
    let rem = secs.rem_euclid(86_400);
    match crate::civil_date::CivilDate::from_unix_timestamp(secs) {
        Some(date) => format!(
            "{} {:02}:{:02}:{:02}",
            date.iso(),
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        ),
        None => format!("{unix_ms} ms since the epoch"),
    }
}

/// `record.hotpath` rendered by the typed rows' printer: the hotpath JSON
/// report as the profiled program wrote it. `Err` with the value as compact
/// JSON when nothing in it is a recognizable report.
fn hotpath_report(
    record: &serde_json::Map<String, serde_json::Value>,
    top: usize,
) -> Option<Result<String, String>> {
    let value = record.get("hotpath")?;
    Some(
        crate::db::hotpath_data_from_json(value)
            .and_then(|data| crate::hotpath_fmt::format_hotpath_report(&data, top))
            .ok_or_else(|| value.to_string()),
    )
}

#[cfg(test)]
mod tests;
