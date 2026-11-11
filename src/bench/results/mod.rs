//! `brokkr results` over the bench store: the current project's script-bench
//! runs, listed, looked up by uuid and compared between two commits, beside
//! the typed rows of `results.db` (`docs/commands/results.md`, Script
//! benches). The store answers the queries (`store::RunFilter`), locally or
//! on the results server the user-wide config names (`super::remote`); this
//! module maps `results`' filters onto them and renders what comes back.

mod annotations;
mod compare;
mod config_diff;
mod single;
mod table;

pub(crate) use compare::format_compare;
pub(crate) use single::{ending as child_ending, format_single};
pub(crate) use table::format_table;

use super::store::{self, RunFilter, StoredRun, wire};
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
    /// `--hidden`: retired and invalidated runs are listed and compared too.
    pub(crate) hidden: bool,
    pub(crate) limit: usize,
}

/// Runs selected together, with the marks they were selected under: every
/// run shown is rendered against those marks, never a later read's. The
/// default is no runs and no marks.
#[derive(Default)]
pub(crate) struct Listing {
    pub(crate) runs: Vec<StoredRun>,
    pub(crate) marks: store::MarkView,
}

/// Both sides of a comparison, the second pinned to the first's revision,
/// and the one set of marks both were selected under.
#[derive(Default)]
pub(crate) struct Comparison {
    pub(crate) a: Vec<StoredRun>,
    pub(crate) b: Vec<StoredRun>,
    pub(crate) marks: store::MarkView,
}

/// The per-user bench store, opened for reading on behalf of one project:
/// the local file, or the results server the user-wide config names.
pub(crate) struct BenchReader {
    backend: Backend,
    project: String,
}

enum Backend {
    Local {
        store: store::Store,
        /// The store's file, named by every error.
        path: std::path::PathBuf,
    },
    Remote(super::remote::Client),
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
    /// The marks the runs were selected under, which say which are hidden.
    pub(crate) marks: store::MarkView,
}

/// What a uuid prefix names among the project's runs ([`BenchReader::resolve`]).
pub(crate) enum Resolved {
    /// Exactly one run, its children loaded.
    One(Box<StoredRun>),
    /// Several runs' uuids, in the store's order.
    Ambiguous(Vec<String>),
    /// None of this project's; other projects' runs it names, as
    /// `(uuid, project)`.
    Missing { elsewhere: Vec<(String, String)> },
}

fn named(path: &std::path::Path, error: &store::StoreError) -> DevError {
    DevError::Database(format!("{}: {error}", path.display()))
}

/// A remote answer of another kind than the request asked for.
fn mismatched(client: &super::remote::Client, asked: &str, answer: &wire::Answer) -> DevError {
    let kind = serde_json::to_value(answer)
        .ok()
        .and_then(|v| v.as_object().and_then(|o| o.keys().next().cloned()))
        .unwrap_or_else(|| "something else".into());
    DevError::Database(format!(
        "results server {}: a {asked} request was answered {kind}",
        client.shown()
    ))
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
    /// Open the bench store for `project`: the results server `server`
    /// names when there is one, which is never probed here (a first query
    /// that cannot be answered fails the command), else the local file.
    /// `None` when there is no local store: no file, or no data home to name
    /// one in. A store that exists but cannot be read (a newer schema, a
    /// foreign file) is an error.
    pub(crate) fn open(
        project: &str,
        server: Option<&crate::config::ResultsServer>,
    ) -> Result<Option<Self>, DevError> {
        if let Some(server) = server {
            return Ok(Some(Self::remote(
                super::remote::Client::new(server),
                project,
            )));
        }
        let Ok(path) = super::evidence::store_path() else {
            return Ok(None);
        };
        Self::at(&path, project)
    }

    /// [`Self::open`] on the local store at `path`.
    pub(crate) fn at(path: &std::path::Path, project: &str) -> Result<Option<Self>, DevError> {
        let store = store::Store::open_read_only(path).map_err(|e| named(path, &e))?;
        Ok(store.map(|store| Self {
            backend: Backend::Local {
                store,
                path: path.to_owned(),
            },
            project: project.to_owned(),
        }))
    }

    /// [`Self::open`] through `client`.
    pub(crate) fn remote(client: super::remote::Client, project: &str) -> Self {
        Self {
            backend: Backend::Remote(client),
            project: project.to_owned(),
        }
    }

    /// Whether the project has any bench run, whatever its outcome.
    pub(crate) fn has_runs(&self) -> Result<bool, DevError> {
        match &self.backend {
            Backend::Local { store, path } => {
                store.has_runs(&self.project).map_err(|e| named(path, &e))
            }
            Backend::Remote(client) => {
                match client.request(&wire::Request::HasRuns {
                    project: self.project.clone(),
                })? {
                    wire::Answer::HasRuns(has) => Ok(has),
                    other => Err(mismatched(client, "has_runs", &other)),
                }
            }
        }
    }

    /// The uuids `filter` selects now.
    fn select(&self, filter: &RunFilter) -> Result<Vec<String>, DevError> {
        Ok(self.select_at(filter, None)?.uuids)
    }

    /// The runs `filter` selects at revision `at` (the current one when
    /// `None`), with the revision and the marks in effect at it.
    fn select_at(
        &self,
        filter: &RunFilter,
        at: Option<store::Revision>,
    ) -> Result<store::Selection, DevError> {
        match &self.backend {
            Backend::Local { store, path } => store.select(filter, at).map_err(|e| named(path, &e)),
            Backend::Remote(client) => {
                match client.request(&wire::Request::Select {
                    filter: filter.clone(),
                    at,
                })? {
                    wire::Answer::Selected(selection) => Ok(selection),
                    other => Err(mismatched(client, "select", &other)),
                }
            }
        }
    }

    /// One run, children on request. A remote answer must say it read
    /// exactly what was asked: a summary passed off as a whole run, or the
    /// reverse, is refused.
    fn run(&self, uuid: &str, children: bool) -> Result<Option<StoredRun>, DevError> {
        match &self.backend {
            Backend::Local { store, path } => {
                store.run(uuid, children).map_err(|e| named(path, &e))
            }
            Backend::Remote(client) => {
                match client.request(&wire::Request::Run {
                    uuid: uuid.to_owned(),
                    children,
                })? {
                    wire::Answer::Run(run) => match run {
                        Some(run) if run.children_loaded != children => {
                            Err(DevError::Database(format!(
                                "results server {}: run {uuid} came back with children_loaded {}, asked {children}",
                                client.shown(),
                                run.children_loaded
                            )))
                        }
                        run => Ok(run.map(|run| *run)),
                    },
                    other => Err(mismatched(client, "run", &other)),
                }
            }
        }
    }

    /// Other projects' runs `prefix` names, as `(uuid, project)`.
    pub(crate) fn elsewhere(&self, prefix: &str) -> Result<Vec<(String, String)>, DevError> {
        match &self.backend {
            Backend::Local { store, path } => store
                .elsewhere(&self.project, prefix)
                .map_err(|e| named(path, &e)),
            Backend::Remote(client) => {
                match client.request(&wire::Request::Elsewhere {
                    project: self.project.clone(),
                    prefix: prefix.to_owned(),
                })? {
                    wire::Answer::Elsewhere(runs) => Ok(runs),
                    other => Err(mismatched(client, "elsewhere", &other)),
                }
            }
        }
    }

    /// Where the store is, as an error names it.
    fn shown(&self) -> String {
        match &self.backend {
            Backend::Local { path, .. } => path.display().to_string(),
            Backend::Remote(client) => format!("results server {}", client.shown()),
        }
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
            hidden: filter.hidden,
            ..RunFilter::default()
        }
    }

    fn load(&self, uuids: &[String], children: bool) -> Result<Vec<StoredRun>, DevError> {
        uuids
            .iter()
            .map(|uuid| {
                self.run(uuid, children)?.ok_or_else(|| {
                    DevError::Database(format!(
                        "{}: bench run {uuid} vanished while reading",
                        self.shown()
                    ))
                })
            })
            .collect()
    }

    /// The newest runs the filter selects, at most `filter.limit`.
    pub(crate) fn table(&self, filter: &BenchFilter) -> Result<Listing, DevError> {
        if filter.excluded {
            return Ok(Listing {
                runs: Vec::new(),
                marks: store::MarkView::default(),
            });
        }
        if let Some(commit) = &filter.commit {
            require_prefix(commit, "commit")?;
        }
        let mut selection = self.select_at(&self.filter(filter), None)?;
        selection.uuids.truncate(filter.limit);
        Ok(Listing {
            runs: self.load(&selection.uuids, false)?,
            marks: selection.marks,
        })
    }

    /// The runs whose uuid starts with `prefix`, filtered and capped.
    pub(crate) fn lookup(&self, prefix: &str, filter: &BenchFilter) -> Result<Lookup, DevError> {
        require_prefix(prefix, "uuid")?;
        // A uuid names a run whatever marks hide it.
        let unfiltered = RunFilter {
            project: self.project.clone(),
            uuid_prefix: Some(prefix.to_owned()),
            hidden: true,
            ..RunFilter::default()
        };
        // The filtered selection is pinned to the candidates' revision, so a
        // run published between the two is in neither.
        let unfiltered = self.select_at(&unfiltered, None)?;
        let candidates = unfiltered.uuids.len();
        let mut uuids = if filter.excluded {
            Vec::new()
        } else {
            self.select_at(
                &RunFilter {
                    uuid_prefix: Some(prefix.to_owned()),
                    hidden: true,
                    ..self.filter(filter)
                },
                Some(unfiltered.revision),
            )?
            .uuids
        };
        let omitted = uuids.len().saturating_sub(filter.limit);
        uuids.truncate(filter.limit);
        let elsewhere = if candidates == 0 {
            self.elsewhere(prefix)?
        } else {
            Vec::new()
        };
        Ok(Lookup {
            runs: self.load(&uuids, true)?,
            omitted,
            candidates,
            elsewhere,
            marks: unfiltered.marks,
        })
    }

    /// Every run of the project whose uuid starts with `prefix`, hidden or
    /// not, in the store's order: the identity a uuid prefix names, read
    /// from the selection alone, no run loaded.
    pub(crate) fn named_by(&self, prefix: &str) -> Result<Vec<String>, DevError> {
        require_prefix(prefix, "uuid")?;
        self.select(&RunFilter {
            project: self.project.clone(),
            uuid_prefix: Some(prefix.to_owned()),
            hidden: true,
            ..RunFilter::default()
        })
    }

    /// What `prefix` names among the project's runs, for a command that reads
    /// one run's evidence (`brokkr sidecar`): the run with its children when
    /// it names exactly one, the candidates when several, and otherwise the
    /// other projects' runs it names. Only one run is ever read whole.
    pub(crate) fn resolve(&self, prefix: &str) -> Result<Resolved, DevError> {
        let mut uuids = self.named_by(prefix)?;
        match uuids.len() {
            0 => Ok(Resolved::Missing {
                elsewhere: self.elsewhere(prefix)?,
            }),
            1 => {
                let uuid = uuids.remove(0);
                let mut runs = self.load(std::slice::from_ref(&uuid), true)?;
                Ok(Resolved::One(Box::new(runs.remove(0))))
            }
            _ => Ok(Resolved::Ambiguous(uuids)),
        }
    }

    /// Each side of a comparison: the completed runs of a clean tree whose
    /// commit starts with that side's prefix and pass the filters, newest
    /// first and uncapped (pairs are formed before `--limit` applies). A run
    /// under a profiled or incidental key is read with its children, where
    /// its captures are; a timing run captured nothing, so it is not.
    pub(crate) fn compare_sides(
        &self,
        commit_a: &str,
        commit_b: &str,
        filter: &BenchFilter,
    ) -> Result<Comparison, DevError> {
        require_prefix(commit_a, "commit")?;
        require_prefix(commit_b, "commit")?;
        if filter.excluded {
            return Ok(Comparison {
                a: Vec::new(),
                b: Vec::new(),
                marks: store::MarkView::default(),
            });
        }
        // The second side is pinned to the first's revision, so both see one
        // history of runs and marks.
        let side = |commit: &str,
                    at: Option<store::Revision>|
         -> Result<(Vec<StoredRun>, store::Selection), DevError> {
            let selection = self.select_at(
                &RunFilter {
                    commit_prefix: Some(commit.to_owned()),
                    with_result: true,
                    clean: true,
                    ..self.filter(filter)
                },
                at,
            )?;
            // With children, every run: what the pairing key leaves out and
            // a pair names (each child's executable and page-cache
            // preparation) lives on them, whatever the run's class.
            let runs = self.load(&selection.uuids, true)?;
            Ok((runs, selection))
        };
        let (a, first) = side(commit_a, None)?;
        let (b, _) = side(commit_b, Some(first.revision))?;
        Ok(Comparison {
            a,
            b,
            marks: first.marks,
        })
    }
}

/// Why `marks` hide a run, each mark with what it came from: `retired` and
/// `invalidated`, in that order, empty for a visible run.
fn hidden_by<'m>(
    marks: &'m store::MarkView,
    run: &StoredRun,
) -> Vec<(&'static str, &'m store::MarkOrigin)> {
    let mut by = Vec::new();
    if let Some(origin) = marks.retired(&run.doc.bench) {
        by.push(("retired", origin));
    }
    if let Some(origin) = marks.invalidated(&run.doc.uuid) {
        by.push(("invalidated", origin));
    }
    by
}

/// The name a run lists, filters, pairs and compares under: the end of its
/// stored bench's rename chain in `marks`.
fn bench_name<'m>(marks: &'m store::MarkView, run: &'m StoredRun) -> &'m str {
    marks.name(&run.doc.bench)
}

/// The run's stored bench name when a rename makes it differ from
/// [`bench_name`]: the stored name is evidence (its config table is
/// `config.bench.STORED`), so every view that shows the resolved name
/// discloses it.
fn stored_as<'r>(marks: &store::MarkView, run: &'r StoredRun) -> Option<&'r str> {
    (bench_name(marks, run) != run.doc.bench).then_some(run.doc.bench.as_str())
}

/// A hiding mark as the single view and `--compare` word it: `retired at
/// TIME on HOST: REASON (mark 8HEX)`.
fn hidden_detail(kind: &str, origin: &store::MarkOrigin) -> String {
    format!(
        "{kind} at {} on {}: {} (mark {})",
        timestamp(origin.at_unix_ms),
        origin.hostname,
        origin.reason,
        crate::db::short_uuid(&origin.uuid)
    )
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

/// The result's wall in microseconds: the measured child's when the run
/// marked one, else the block's.
fn result_wall(run: &StoredRun) -> Option<i64> {
    run.doc
        .iterations
        .get(run.best_iteration?)?
        .result_wall_us()
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
pub(crate) fn timestamp(unix_ms: i64) -> String {
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

// Visible to the rest of the crate: the remote client tests and `brokkr
// sidecar`'s read runs built by its fixtures.
#[cfg(test)]
pub(crate) mod tests;
