use std::path::Path;

use crate::config::project_config::ratatoskr::RatatoskrConfig;
use crate::db::QueryFilter;
use crate::error::DevError;
/// One selected result view after the CLI's mutually-exclusive selectors and
/// fixed-width compare operands have been resolved.
pub(crate) enum ResultsView {
    Table,
    Uuid { prefix: String },
    Compare { commits: (String, String) },
}

/// The complete request for the `results` command.
pub(crate) struct ResultsQuery {
    pub(crate) view: ResultsView,
    pub(crate) filter: QueryFilter,
    pub(crate) top: usize,
}

/// Query parameters for the `corpus-results` command (piners only) - the
/// corpus run store (`.brokkr/piners/corpus/runs.db`). Distinct from
/// [`ResultsQuery`]: no benchmark filters apply here, so the bench store and
/// the corpus store no longer share one overloaded command.
pub(crate) struct CorpusQuery {
    /// Bare positional run id (default: latest). Equivalent to `--run`.
    pub(crate) run_id: Option<i64>,
    /// `--run N`: a specific corpus run id (default: latest).
    pub(crate) run: Option<i64>,
    /// Row cap for the recent-runs table and `--trend` history.
    pub(crate) limit: usize,
    /// Probe selector. One id (no table flag) is the combo view; repeated
    /// under `--diffs`/`--dispositions` is an IN-list filter on that table.
    pub(crate) probe: Vec<String>,
    pub(crate) diffs: bool,
    pub(crate) dispositions: bool,
    /// `--columns` projection for the `--diffs`/`--dispositions` table. Empty
    /// = that table's curated default; `["all"]` = every column, rendered
    /// vertically; else a validated subset.
    pub(crate) columns: Vec<String>,
    /// `--runtimes`: per-probe most-recent runtime, slowest first.
    pub(crate) runtimes: bool,
    /// `--over <secs>`: with `--runtimes`, keep only probes above this many
    /// seconds.
    pub(crate) over: Option<f64>,
    pub(crate) trend: Option<String>,
    pub(crate) where_expr: Option<String>,
    pub(crate) sql: Option<String>,
    /// Run-detail view: show every probe, not just the ones deviating from
    /// their pin. With `compare`, also list the probes that did not move.
    pub(crate) full: bool,
    /// `--compare A B`: the per-probe count comparison, B read against A.
    pub(crate) compare: Option<(i64, i64)>,
}

/// One selected sidecar view. Selectors are resolved before the command
/// handler receives the request, and compare operands are always a pair.
pub(crate) enum SidecarView {
    Summary { uuid: String },
    Samples { uuid: String },
    Markers { uuid: String },
    Durations { uuid: String },
    Counters { uuid: String },
    Stalls { uuid: String },
    Stat { uuid: String },
    Compare { uuids: (String, String) },
}

/// Query parameters for the `sidecar` command.
pub(crate) struct SidecarQuery {
    pub(crate) view: SidecarView,
    /// The stat field is also kept here for timeline-filter validation.
    pub(crate) stat: Option<String>,
    pub(crate) human: bool,
    pub(crate) run: Option<String>,
    pub(crate) phase: Option<String>,
    /// Substring filter on counter names, for the `--counters` view.
    pub(crate) grep: Option<String>,
    pub(crate) range: Option<String>,
    pub(crate) where_cond: Option<String>,
    pub(crate) fields: Vec<String>,
    pub(crate) every: Option<usize>,
    pub(crate) head: Option<usize>,
    pub(crate) tail: Option<usize>,
}

/// Fixed-width comparison operands while argv is still at the CLI boundary.
pub(crate) fn compare_pair<T>(values: Vec<T>, option: &str) -> Result<(T, T), DevError> {
    let [first, second]: [T; 2] = values
        .try_into()
        .map_err(|_| DevError::Usage(format!("{option} requires exactly two values")))?;
    Ok((first, second))
}

/// Service entry-point inputs. Keeping invocation state together lets the
/// ratatoskr service entry points accept one explicit request value.
pub(crate) struct ServiceSuiteRequest<'a> {
    pub(crate) project_root: &'a Path,
    /// The code tree the harness is built from (cwd); differs from
    /// `project_root` only when `brokkr.toml` lives one level up.
    pub(crate) build_root: &'a crate::project::BuildRoot,
    /// The composed config's `include` files, exempt from the clean-tree
    /// flag each run's `run.toml` records.
    pub(crate) included_config_files: &'a [std::path::PathBuf],
    pub(crate) ratatoskr: &'a RatatoskrConfig,
    pub(crate) filter: Option<&'a str>,
    pub(crate) keep_artefacts: bool,
    pub(crate) profile_override: Option<bool>,
    pub(crate) keep_going: bool,
    pub(crate) include_ignored: bool,
    pub(crate) repeat: std::num::NonZeroU32,
}

pub(crate) struct ServiceTestRequest<'a> {
    pub(crate) project_root: &'a Path,
    /// The code tree the harness is built from; see [`ServiceSuiteRequest`].
    pub(crate) build_root: &'a crate::project::BuildRoot,
    /// See [`ServiceSuiteRequest`].
    pub(crate) included_config_files: &'a [std::path::PathBuf],
    pub(crate) ratatoskr: &'a RatatoskrConfig,
    pub(crate) script: &'a str,
    pub(crate) keep_artefacts: bool,
    pub(crate) profile_override: Option<bool>,
    pub(crate) repeat: std::num::NonZeroU32,
    pub(crate) keep_going: bool,
}
