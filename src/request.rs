use crate::db::QueryFilter;
use crate::error::DevError;

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
    /// Negative `--grep-v`: a row is excluded when any term matches either
    /// invocation column. ORs together (any hit excludes), which is the
    /// natural dual of `grep`'s AND.
    pub(crate) grep_v: Vec<String>,
    pub(crate) limit: usize,
    pub(crate) top: usize,
}

/// The mutually exclusive result views after the CLI request has been
/// validated and its row filters have been assembled once.
pub(crate) enum ResultsView {
    Table {
        filter: QueryFilter,
    },
    Uuid {
        prefix: String,
        filter: QueryFilter,
        top: usize,
    },
    Compare {
        commits: (String, String),
        filter: QueryFilter,
        top: usize,
    },
}

impl ResultsQuery {
    pub(crate) fn view(&self) -> Result<ResultsView, DevError> {
        let filter = QueryFilter {
            commit: self.commit.clone(),
            command: self.command.clone(),
            mode: self.mode.clone(),
            dataset: self.dataset.clone(),
            meta: self
                .meta
                .iter()
                .filter_map(|s| {
                    s.split_once('=')
                        .map(|(key, value)| (key.to_owned(), value.to_owned()))
                })
                .collect(),
            env: self
                .env
                .iter()
                .filter_map(|s| {
                    s.split_once('=')
                        .map(|(key, value)| (key.to_owned(), value.to_owned()))
                })
                .collect(),
            grep: self.grep.clone(),
            grep_v: self.grep_v.clone(),
            limit: self.limit,
        };

        match (self.query.as_deref(), self.compare.as_deref()) {
            (Some(prefix), None) if filter.commit.is_none() => Ok(ResultsView::Uuid {
                prefix: prefix.to_owned(),
                filter,
                top: self.top,
            }),
            (None, Some(commits)) if filter.commit.is_none() => {
                let [a, b] = commits else {
                    return Err(DevError::Config(
                        "--compare requires exactly two commits".into(),
                    ));
                };
                Ok(ResultsView::Compare {
                    commits: (a.clone(), b.clone()),
                    filter,
                    top: self.top,
                })
            }
            (None, None) => Ok(ResultsView::Table { filter }),
            _ => Err(DevError::Config(
                "results accepts one of a UUID, --compare, or --commit".into(),
            )),
        }
    }
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
    /// Substring filter on counter names, for the `--counters` view.
    pub(crate) grep: Option<String>,
    pub(crate) range: Option<String>,
    pub(crate) where_cond: Option<String>,
    pub(crate) fields: Vec<String>,
    pub(crate) every: Option<usize>,
    pub(crate) head: Option<usize>,
    pub(crate) tail: Option<usize>,
}

/// One selected sidecar output. The query UUID and any stat field travel with
/// their view, so dispatch cannot accidentally choose a result while dropping
/// its selector.
pub(crate) enum SidecarView<'a> {
    Summary { uuid: &'a str },
    Samples { uuid: &'a str },
    Markers { uuid: &'a str },
    Durations { uuid: &'a str },
    Counters { uuid: &'a str },
    Stalls { uuid: &'a str },
    Stat { uuid: &'a str, field: &'a str },
    Compare { uuids: (&'a str, &'a str) },
}

impl SidecarQuery {
    pub(crate) fn view(&self) -> Result<SidecarView<'_>, DevError> {
        if let Some(uuids) = self.compare.as_deref() {
            let [a, b] = uuids else {
                return Err(DevError::Config(
                    "--compare requires exactly two UUIDs".into(),
                ));
            };
            return Ok(SidecarView::Compare {
                uuids: (a.as_str(), b.as_str()),
            });
        }

        let uuid = self.query.as_deref().ok_or_else(|| {
            DevError::Config("sidecar requires a UUID unless --compare is used".into())
        })?;
        Ok(if self.samples {
            SidecarView::Samples { uuid }
        } else if self.markers {
            SidecarView::Markers { uuid }
        } else if self.durations {
            SidecarView::Durations { uuid }
        } else if self.counters {
            SidecarView::Counters { uuid }
        } else if self.stalls {
            SidecarView::Stalls { uuid }
        } else if let Some(field) = self.stat.as_deref() {
            SidecarView::Stat { uuid, field }
        } else {
            SidecarView::Summary { uuid }
        })
    }
}
