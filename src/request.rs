/// Query parameters for the results command.
pub(crate) struct ResultsQuery {
    pub(crate) query: Option<String>,
    pub(crate) commit: Option<String>,
    pub(crate) compare: Option<Vec<String>>,
    pub(crate) compare_last: bool,
    pub(crate) command: Option<String>,
    pub(crate) mode: Option<String>,
    pub(crate) dataset: Option<String>,
    /// Metadata filters as `key=value` strings, parsed into `(key, value)`
    /// pairs by `cmd_results`. Multiple filters AND together.
    pub(crate) meta: Vec<String>,
    /// Substring match against the `cli_args` OR `brokkr_args` columns
    /// (unified `--grep`, à la `git log --grep`).
    pub(crate) grep: Option<String>,
    pub(crate) limit: usize,
    pub(crate) top: usize,
    pub(crate) timeline: bool,
    pub(crate) markers: bool,
    pub(crate) summary: bool,
    pub(crate) durations: bool,
    pub(crate) fields: Vec<String>,
    pub(crate) every: Option<usize>,
    pub(crate) head: Option<usize>,
    pub(crate) tail: Option<usize>,
    pub(crate) where_cond: Option<String>,
    pub(crate) stat: Option<String>,
    pub(crate) phase: Option<String>,
    pub(crate) range: Option<String>,
    pub(crate) run: Option<String>,
    pub(crate) compare_timeline: Option<Vec<String>>,
    pub(crate) phases: bool,
    pub(crate) counters: bool,
    /// Render human-friendly table output instead of the default JSONL.
    /// Applies to `--timeline --summary` and `--compare-timeline`.
    pub(crate) human: bool,
}
