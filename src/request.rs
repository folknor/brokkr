/// Query parameters for the `results` command.
pub(crate) struct ResultsQuery {
    pub(crate) query: Option<String>,
    pub(crate) commit: Option<String>,
    pub(crate) compare: Option<Vec<String>>,
    pub(crate) command: Option<String>,
    pub(crate) mode: Option<String>,
    pub(crate) dataset: Option<String>,
    /// Metadata filters as `key=value` strings, parsed into `(key, value)`
    /// pairs by `cmd_results`. Multiple filters AND together.
    pub(crate) meta: Vec<String>,
    /// Captured-env filters as `KEY=VALUE` strings, parsed by
    /// `cmd_results`. Multiple filters AND. Rows without the key are
    /// excluded (no missing-as-0 coercion).
    pub(crate) env: Vec<String>,
    /// Substring match against the `cli_args` OR `brokkr_args` columns
    /// (unified `--grep`, à la `git log --grep`). Multiple terms AND
    /// together - each must match the row.
    pub(crate) grep: Vec<String>,
    pub(crate) limit: usize,
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
    /// Probe selector. One id (no `--diffs`) is the combo view; repeated under
    /// `--diffs` is an IN-list filter on the diff table.
    pub(crate) probe: Vec<String>,
    pub(crate) diffs: bool,
    /// `--columns` projection for the `--diffs` table. Empty = curated default;
    /// `["all"]` = every column, rendered vertically; else a validated subset.
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
    /// their pin. Ignored by the other views.
    pub(crate) full: bool,
}

/// Query parameters for the `sidecar` command.
///
/// Each `SidecarQuery` picks exactly one view (the default is the
/// per-phase summary when no selector is set). `compare` takes two
/// UUIDs instead of the single `query` UUID; clap enforces that.
pub(crate) struct SidecarQuery {
    pub(crate) query: Option<String>,
    pub(crate) samples: bool,
    pub(crate) markers: bool,
    pub(crate) durations: bool,
    pub(crate) counters: bool,
    pub(crate) stalls: bool,
    pub(crate) stat: Option<String>,
    pub(crate) compare: Option<Vec<String>>,
    pub(crate) human: bool,
    pub(crate) run: Option<String>,
    pub(crate) phase: Option<String>,
    pub(crate) range: Option<String>,
    pub(crate) where_cond: Option<String>,
    pub(crate) fields: Vec<String>,
    pub(crate) every: Option<usize>,
    pub(crate) head: Option<usize>,
    pub(crate) tail: Option<usize>,
}
