//! Implementation of the `sidecar` command - per-phase summary, raw
//! sample/marker export, START/END durations, counters, single-field
//! stats, and two-result phase compare. Each invocation picks exactly
//! one view; clap enforces the mutual exclusion.
//!
//! A uuid prefix resolves across the typed stores (results.db and
//! sidecar.db) and the project's script-bench runs at once; a prefix naming
//! runs in both is ambiguous. A typed run is read as it always was; a bench
//! run is read one child of each selected iteration at a time (`bench.rs`),
//! and the views (`views.rs`) are written once over either (`source.rs`).

mod bench;
mod source;
mod views;

use std::path::Path;
use std::sync::OnceLock;

use crate::bench::results::{BenchReader, Resolved as BenchResolved};
use crate::bench::store::StoredRun;
use crate::config::DevConfig;
use crate::db;
use crate::db::paths::{results_db_path, sidecar_db_path};
use crate::error::DevError;
use crate::output;
use crate::request::{SidecarQuery, SidecarView};
use crate::sidecar_fmt::{CompareSide, Process, print_compare_timeline, validate_timeline_args};
use source::Source;

/// A compact, current list of sidecar view selectors for results output.
/// Selectors are the options whose clap conflict set contains the other view
/// choices; command modifiers such as `--run`, `--phase` and `--human` stay out
/// of the hint automatically.
pub(crate) fn view_selector_hint() -> &'static str {
    static HINT: OnceLock<String> = OnceLock::new();
    HINT.get_or_init(|| {
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(|| {
                use clap::CommandFactory;

                let mut cli = crate::cli::Cli::command();
                let Some(command) = cli.find_subcommand_mut("sidecar") else {
                    return "see `brokkr sidecar --help` for available views".to_owned();
                };
                let selectors = command
                    .get_arguments()
                    .filter(|arg| arg.get_long().is_some())
                    .filter(|arg| {
                        command
                            .get_arg_conflicts_with(arg)
                            .iter()
                            .filter(|other| other.get_long().is_some())
                            .count()
                            >= 3
                    })
                    .filter_map(|arg| arg.get_long().map(|long| format!("--{long}")))
                    .collect::<Vec<_>>();
                if selectors.is_empty() {
                    "see `brokkr sidecar --help` for available views".to_owned()
                } else {
                    format!("views: {}", selectors.join(", "))
                }
            })
            .ok()
            .and_then(|worker| worker.join().ok())
            .unwrap_or_else(|| "see `brokkr sidecar --help` for available views".to_owned())
    })
}

/// Resolve `--run`: `"all"` → None (all runs), N → Some(N), absent → best run.
/// Also logs which run(s) will be shown when the result has multiple runs.
fn resolve_run_filter(
    store: &db::RunStore,
    id: &db::RunId,
    run_arg: Option<&str>,
) -> Result<Option<usize>, DevError> {
    let (best_idx, total) = store.query_sidecar_meta(id)?;
    let run_filter = match run_arg {
        Some("all") => None,
        Some(n) => Some(n.parse::<usize>().map_err(|_| {
            DevError::Usage(format!("--run: expected a number or 'all', got '{n}'"))
        })?),
        None => Some(best_idx),
    };
    warn_about_stored_capture(store, id, run_filter)?;
    if total > 1 {
        let showing = match run_filter {
            Some(idx) => format!("run {idx}/{total}"),
            None => format!("all {total} runs"),
        };
        output::sidecar_msg(&format!("showing {showing} (use --run to override)"));
    }
    Ok(run_filter)
}

fn warn_about_stored_capture(
    store: &db::RunStore,
    id: &db::RunId,
    run_idx: Option<usize>,
) -> Result<(), DevError> {
    if store.query_sidecar_capture_truncated(id, run_idx)? {
        output::warn(
            "stored sidecar trajectory is incomplete: a capture limit was reached, FIFO \
             lines were rejected or left unread, child output was cut, or (for a run stored \
             before sidecar.db v10) sink-end records had no place to be stored",
        );
    }
    // A separate fact from truncation: every record may have arrived intact
    // and still sit off the sample clock. One line per run, so `--run all`
    // and `--compare` never fold several runs' counts into one.
    for (run, outliers) in store.query_sidecar_clock_outliers(id, run_idx)? {
        if let Some(message) = outliers.message(&format!("sidecar {} --run {run}", id.as_str())) {
            output::warn(&message);
        }
    }
    Ok(())
}

/// The stores a query resolves against: the typed ones, and the bench store
/// the project's script-bench runs live in, locally or on the configured
/// results server.
struct Stores {
    typed: db::RunStore,
    bench: Option<BenchReader>,
}

pub(crate) fn cmd_sidecar(
    dev_config: &DevConfig,
    project_root: &Path,
    project: &str,
    q: &SidecarQuery,
) -> Result<(), DevError> {
    // Malformed filters fail before anything is printed: `--where`/`--fields`
    // used to fail open (a bad condition printed every sample, an unknown
    // field silently vanished), which reads as a real answer.
    validate_timeline_args(q)?;
    // litehtml's `.brokkr/results.db` is its mechanical database, another
    // schema: it has no typed runs, only script-bench runs.
    let typed = if project == crate::project::Project::Litehtml.name() {
        db::RunStore::absent()
    } else {
        db::RunStore::open_read_only(
            &results_db_path(project_root),
            &sidecar_db_path(project_root),
        )?
    };
    // An unreadable bench store, or an unreachable server, fails the query as
    // it fails `brokkr results`: an unanswerable history is not an empty one.
    let bench = BenchReader::open(project, dev_config.results_server.as_ref())?;
    if !typed.has_sidecar_db() && bench.is_none() {
        output::result_msg("no sidecar.db found");
        return Ok(());
    }
    let stores = Stores { typed, bench };
    let uuid = match &q.view {
        SidecarView::Compare { uuids: (a, b) } => return run_compare(&stores, a, b, q.human),
        SidecarView::Summary { uuid }
        | SidecarView::Samples { uuid }
        | SidecarView::Markers { uuid }
        | SidecarView::Durations { uuid }
        | SidecarView::Counters { uuid }
        | SidecarView::Stalls { uuid }
        | SidecarView::Stat { uuid } => uuid,
    };
    with_resolved(&stores, uuid, q, |src| match &q.view {
        SidecarView::Summary { .. } => views::phase_summary(src, q),
        SidecarView::Samples { .. } => views::samples(src, q),
        SidecarView::Markers { .. } => views::markers(src, q),
        SidecarView::Durations { .. } => views::durations(src, q),
        SidecarView::Counters { .. } => views::counters(src, q),
        SidecarView::Stalls { .. } => views::stalls(src, q),
        SidecarView::Stat { .. } => {
            let field = q
                .stat
                .as_deref()
                .ok_or_else(|| DevError::Usage("sidecar stat view requires a field".into()))?;
            views::stat(src, field, q)
        }
        SidecarView::Compare { .. } => Ok(()),
    })
}

/// What a prefix names across every store.
enum Found {
    Typed(db::RunId),
    Bench(Box<StoredRun>),
    /// A typed run, but there is no sidecar.db to read it from.
    NoSidecarDb,
    /// Nothing of this project's; other projects' bench runs it names.
    Missing {
        elsewhere: Vec<(String, String)>,
    },
}

/// Resolve `prefix` against the typed stores and the bench store together:
/// a typed candidate counts whether or not it has sidecar data, so the
/// answer never depends on which store has something to render. The typed
/// latest-key aliases (`dirty`) resolve first, as they always have.
fn resolve(stores: &Stores, prefix: &str) -> Result<Found, DevError> {
    let typed = match stores.typed.resolve(db::RunRef::Uuid(prefix))? {
        db::Resolved::One(id) => vec![id],
        db::Resolved::Ambiguous(ids) => ids,
        db::Resolved::Missing => Vec::new(),
    };
    let bench = match &stores.bench {
        Some(reader) => reader.resolve(prefix)?,
        None => BenchResolved::Missing {
            elsewhere: Vec::new(),
        },
    };
    let bench_uuids: Vec<String> = match &bench {
        BenchResolved::One(run) => vec![run.doc.uuid.clone()],
        BenchResolved::Ambiguous(uuids) => uuids.clone(),
        BenchResolved::Missing { .. } => Vec::new(),
    };
    if typed.len() + bench_uuids.len() > 1 {
        return Err(ambiguous(prefix, &typed, &bench_uuids));
    }
    if let Some(id) = typed.into_iter().next() {
        return Ok(if stores.typed.has_sidecar_db() {
            Found::Typed(id)
        } else {
            Found::NoSidecarDb
        });
    }
    match bench {
        BenchResolved::One(run) => Ok(Found::Bench(run)),
        BenchResolved::Ambiguous(_) => Err(ambiguous(prefix, &[], &bench_uuids)),
        BenchResolved::Missing { elsewhere } => {
            if elsewhere.is_empty() && !stores.typed.has_sidecar_db() {
                Ok(Found::NoSidecarDb)
            } else {
                Ok(Found::Missing { elsewhere })
            }
        }
    }
}

/// The candidates of an ambiguous prefix, typed ones first; a bench run is
/// marked as one, so a list naming typed runs alone reads as it always did.
fn ambiguous(prefix: &str, typed: &[db::RunId], bench: &[String]) -> DevError {
    const SHOWN: usize = 10;
    let all: Vec<String> = typed
        .iter()
        .map(|id| id.as_str().to_owned())
        .chain(bench.iter().map(|uuid| format!("{uuid} (script bench)")))
        .collect();
    let more = all.len().saturating_sub(SHOWN);
    let tail = if more > 0 {
        format!(", and {more} more")
    } else {
        String::new()
    };
    let listed: Vec<&str> = all.iter().take(SHOWN).map(String::as_str).collect();
    DevError::Config(format!(
        "uuid prefix '{prefix}' is ambiguous - it matches {} runs: {}{tail}",
        all.len(),
        listed.join(", "),
    ))
}

/// The message for a prefix that names nothing readable here.
fn report_missing(found: &Found, missing: &str) {
    match found {
        Found::NoSidecarDb => output::result_msg("no sidecar.db found"),
        Found::Missing { elsewhere } if !elsewhere.is_empty() => {
            for (uuid, project) in elsewhere {
                output::result_msg(&format!(
                    "script-bench run {} belongs to project {project}; `brokkr sidecar` reads \
                     the current project's runs",
                    db::short_uuid(uuid)
                ));
            }
        }
        _ => output::result_msg(missing),
    }
}

fn with_resolved(
    stores: &Stores,
    prefix: &str,
    q: &SidecarQuery,
    view: impl FnOnce(&Source<'_>) -> Result<(), DevError>,
) -> Result<(), DevError> {
    match resolve(stores, prefix)? {
        Found::Typed(id) => {
            if q.child.is_some() {
                return Err(DevError::Usage(format!(
                    "--child selects a child of a script-bench run's iteration; {} is a typed \
                     run",
                    id.short()
                )));
            }
            view(&Source::Typed {
                store: &stores.typed,
                id,
            })
        }
        Found::Bench(run) => {
            let run = bench::select(*run, q.run.as_deref(), q.child)?;
            if !run.has_child() {
                run.print_header();
                output::result_msg(&run.no_child_message());
                return Ok(());
            }
            view(&Source::Bench(Box::new(run)))
        }
        found => {
            report_missing(&found, "no sidecar data for this result");
            Ok(())
        }
    }
}

/// A `--compare` operand: a uuid prefix, and for a script-bench run
/// optionally `/ITER` and `/ITER/CHILD`.
fn operand(value: &str) -> Result<(&str, Option<usize>, Option<usize>), DevError> {
    let mut parts = value.split('/');
    let prefix = parts.next().unwrap_or_default();
    let mut number = |what: &str| -> Result<Option<usize>, DevError> {
        parts
            .next()
            .map(|n| {
                n.parse::<usize>().map_err(|_| {
                    DevError::Usage(format!(
                        "--compare: '{value}': {what} '{n}' is not a number (UUID, UUID/ITER or \
                         UUID/ITER/CHILD)"
                    ))
                })
            })
            .transpose()
    };
    let iteration = number("iteration")?;
    let child = number("child")?;
    if parts.next().is_some() {
        return Err(DevError::Usage(format!(
            "--compare: '{value}' has more than UUID/ITER/CHILD"
        )));
    }
    Ok((prefix, iteration, child))
}

/// One resolved `--compare` side, before its evidence is read.
enum Side {
    Typed { id: db::RunId, best: usize },
    Bench(Box<bench::BenchRun>),
}

/// Resolve an operand to a side, or print why there is none. A typed side is
/// its best run; a bench side its result iteration unless `/ITER` names
/// another, and the iteration's sole child unless `/CHILD` names one. A
/// bench run without a result never compares, whatever the operand says.
fn compare_side(stores: &Stores, value: &str) -> Result<Option<Side>, DevError> {
    let (prefix, iteration, child) = operand(value)?;
    match resolve(stores, prefix)? {
        Found::Typed(id) => {
            if iteration.is_some() {
                return Err(DevError::Usage(format!(
                    "--compare: '{value}': /ITER and /CHILD select a script-bench run's child; \
                     {prefix} is a typed run, compared at its best run"
                )));
            }
            let (best, _) = stores.typed.query_sidecar_meta(&id)?;
            Ok(Some(Side::Typed { id, best }))
        }
        Found::Bench(run) => {
            let Some(best) = run.best_iteration else {
                return Err(DevError::Config(format!(
                    "--compare: bench run {} has no result; only a run with a result compares",
                    db::short_uuid(&run.doc.uuid)
                )));
            };
            let iteration = iteration.unwrap_or(best).to_string();
            let run = bench::select(*run, Some(&iteration), child)?;
            if !run.has_child() {
                return Err(DevError::Config(format!(
                    "--compare: '{value}': {}",
                    run.no_child_message()
                )));
            }
            Ok(Some(Side::Bench(Box::new(run))))
        }
        found => {
            report_missing(&found, "one or both results have no sidecar data");
            Ok(None)
        }
    }
}

/// A side's evidence: its samples, markers and process.
struct SideData {
    uuid: String,
    child: Option<(usize, usize)>,
    samples: Vec<crate::sidecar::Sample>,
    markers: Vec<crate::sidecar::Marker>,
    process: Process,
}

fn side_data(stores: &Stores, side: &Side) -> Result<SideData, DevError> {
    match side {
        Side::Typed { id, best } => Ok(SideData {
            uuid: id.as_str().to_owned(),
            child: None,
            samples: stores.typed.query_sidecar_samples(id, Some(*best))?,
            markers: Vec::new(),
            process: Process::Sampled,
        }),
        Side::Bench(run) => {
            let (iteration, child) = run.one().ok_or_else(|| {
                DevError::Config("--compare reads one child of one iteration".into())
            })?;
            Ok(SideData {
                uuid: run.uuid().to_owned(),
                child: Some((iteration, child)),
                samples: run.samples(Some(iteration)).0,
                markers: run.markers(Some(iteration)),
                process: run.process(iteration),
            })
        }
    }
}

/// Two sessions' process streams, phase-aligned. Call streams, their
/// boundaries and sink ends are not compared here, deliberately: the view is
/// process-only (`docs/commands/measure.md`, Marker and counter rules).
fn run_compare(stores: &Stores, uuid_a: &str, uuid_b: &str, human: bool) -> Result<(), DevError> {
    // Each side must name exactly one run across every store.
    let Some(a) = compare_side(stores, uuid_a)? else {
        return Ok(());
    };
    let Some(b) = compare_side(stores, uuid_b)? else {
        return Ok(());
    };
    for side in [&a, &b] {
        match side {
            Side::Typed { id, best } => warn_about_stored_capture(&stores.typed, id, Some(*best))?,
            Side::Bench(run) => run.warn_capture(),
        }
    }
    let mut data_a = side_data(stores, &a)?;
    let mut data_b = side_data(stores, &b)?;
    // A typed side without samples has no timeline to compare; a bench
    // child's interval is known whether or not a sample landed in it.
    let empty = |d: &SideData| d.process == Process::Sampled && d.samples.is_empty();
    if empty(&data_a) || empty(&data_b) {
        output::result_msg("one or both results have no sidecar data");
        return Ok(());
    }
    for (side, data) in [(&a, &mut data_a), (&b, &mut data_b)] {
        if let Side::Typed { id, best } = side {
            data.markers = stores.typed.query_sidecar_markers(id, Some(*best))?;
        }
    }
    print_compare_timeline(&data_a.side(), &data_b.side(), human);
    Ok(())
}

impl SideData {
    fn side(&self) -> CompareSide<'_> {
        CompareSide {
            uuid: &self.uuid,
            child: self.child,
            samples: &self.samples,
            markers: &self.markers,
            process: self.process,
        }
    }
}
