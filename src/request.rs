//! The resolved requests of the `results` and `sidecar` commands
//! ([`ResultsQuery`], [`SidecarQuery`]), built from the parsed CLI.

use crate::db::QueryFilter;
use crate::error::DevError;
/// One selected result view after the CLI's mutually-exclusive selectors and
/// fixed-width compare operands have been resolved.
pub(crate) enum ResultsView {
    Table,
    Uuid {
        prefix: String,
    },
    Compare {
        commits: (String, String),
    },
    /// `--invocation ID`: one `--rounds` invocation's summary.
    Invocation {
        prefix: String,
    },
}

/// The complete request for the `results` command.
pub(crate) struct ResultsQuery {
    pub(crate) view: ResultsView,
    pub(crate) filter: QueryFilter,
    /// `--param NAME=VALUE`: script-bench runs only, so a non-empty list
    /// excludes every typed row.
    pub(crate) params: Vec<(String, String)>,
    pub(crate) top: usize,
    /// `--hidden`: script-bench runs that marks hide are listed and compared
    /// too. Typed rows are not marked, so it leaves them as they are.
    pub(crate) hidden: bool,
    /// The detected project's name, which selects its runs in the per-user
    /// bench store.
    pub(crate) project: String,
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
    /// `--child`: the child of each selected iteration of a script-bench run.
    pub(crate) child: Option<usize>,
    pub(crate) phase: Option<String>,
    /// Substring filter on counter names, for the `--counters` view.
    pub(crate) grep: Option<String>,
    /// One call stream; `None` is the process stream plus, where the view
    /// shows them, every call stream.
    pub(crate) call: Option<String>,
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
