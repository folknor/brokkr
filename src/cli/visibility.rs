// Which top-level subcommands apply to which projects.
//
// This file is `include!`d into `src/cli.rs`, so it carries `//` comments
// rather than `//!` - an inner doc comment cannot appear spliced mid-module.
//
// `brokkr` is one binary serving several unrelated projects, so most of its
// subcommand surface is irrelevant in any given checkout - a piners tree has
// no use for `apply-changes`, and a pbfhogg tree has none for `lint-corpus`.
// `TABLE` is the declarative statement of that mapping, keyed by the
// subcommand name clap prints in `--help` (the `#[command(name = "...")]`
// attribute when present, otherwise the kebab-cased variant name).
//
// This table drives *help visibility only*. It is not the gate: the
// authoritative refusal still lives in each handler's
// `project::require(...)` call, which produces the explanatory error when a
// wrong-project command is invoked anyway. Hiding a command never disables
// it, and this table must be kept in agreement with those call sites.
//
// Fail-open rule: `visible_in` returns `true` for any name it does not
// recognise. A subcommand added to the CLI but forgotten here stays visible
// everywhere - the failure mode is a slightly noisy `--help`, never a
// command that silently vanishes from the interface.

use crate::project::Project;

/// Which projects a subcommand applies to.
pub(crate) enum Visibility {
    /// Available everywhere (check, test, clippy, deps, env, results, ...).
    Any,
    /// Only these projects.
    Only(&'static [Project]),
    /// Every project except these. The arm `Only` cannot express: a command
    /// that works in the built-ins *and* in unrecognised `Other(_)` trees but
    /// is refused by a couple of them. Listing the positives instead would
    /// silently drop `Other(_)`, which is the common case for a foreign
    /// checkout driven from a parent `brokkr.toml`.
    Except(&'static [Project]),
}

/// Projects with no rows in `results.db` / `sidecar.db`, so the three commands
/// that read and prune those stores are noise there.
///
/// Litehtml is the only one. It *does* open `results_db_path`, but through
/// `MechanicalDb` - the `mechanical_runs` / `mechanical_results` /
/// `mechanical_approvals` schema for visual-reference runs, sharing the file
/// and nothing else. `brokkr results` reads `ResultsDb` and would find
/// nothing.
///
/// Every other project reaches `BenchHarness`, which is what writes those
/// rows: pbfhogg and elivagar heavily, nidhogg via `bench_{api,tiles}`,
/// ratatoskr via `bench_gate`/`list_smoke`, sluggrs via `hotpath` (so sluggrs
/// is on both lists - `MechanicalDb` for visual work, real result rows from
/// hotpath), piners for its hotpath/alloc runs, and any `Other(_)` tree
/// through the ungated `generic-hotpath`.
const MEASURED_DB_ABSENT: &[Project] = &[Project::Litehtml];

/// Table of every top-level subcommand name -> the projects it applies to.
///
/// Sorted by name. Every variant of `crate::cli::schema::Command` appears
/// exactly once.
pub(crate) const TABLE: &[(&str, Visibility)] = &[
    ("add-locations-to-ways", Visibility::Only(&[Project::Pbfhogg])),
    ("api", Visibility::Only(&[Project::Nidhogg])),
    ("apply-changes", Visibility::Only(&[Project::Pbfhogg])),
    ("approve", Visibility::Only(&[Project::Litehtml, Project::Sluggrs])),
    ("bless", Visibility::Only(&[Project::Elivagar])),
    ("build-geocode-index", Visibility::Only(&[Project::Pbfhogg])),
    ("cat", Visibility::Only(&[Project::Pbfhogg])),
    ("check", Visibility::Any),
    ("check-ids", Visibility::Only(&[Project::Pbfhogg])),
    ("check-refs", Visibility::Only(&[Project::Pbfhogg])),
    ("clean", Visibility::Any),
    ("clippy", Visibility::Any),
    ("compare-tiles", Visibility::Only(&[Project::Elivagar])),
    ("corpus", Visibility::Only(&[Project::Piners])),
    ("corpus-results", Visibility::Only(&[Project::Piners])),
    ("degrade", Visibility::Only(&[Project::Pbfhogg])),
    ("deps", Visibility::Any),
    ("diag", Visibility::Only(&[Project::Elivagar])),
    ("diff", Visibility::Only(&[Project::Pbfhogg])),
    ("diff-snapshots", Visibility::Only(&[Project::Pbfhogg])),
    ("download", Visibility::Only(&[Project::Pbfhogg])),
    ("download-natural-earth", Visibility::Only(&[Project::Elivagar])),
    ("download-ocean", Visibility::Only(&[Project::Elivagar])),
    ("env", Visibility::Any),
    ("extract", Visibility::Only(&[Project::Pbfhogg])),
    ("fmt", Visibility::Any),
    ("generic-hotpath", Visibility::Any),
    ("geocode", Visibility::Only(&[Project::Nidhogg])),
    ("getid", Visibility::Only(&[Project::Pbfhogg])),
    ("getparents", Visibility::Only(&[Project::Pbfhogg])),
    ("history", Visibility::Any),
    ("hotpath", Visibility::Only(&[Project::Sluggrs])),
    ("html-extract", Visibility::Only(&[Project::Litehtml])),
    ("ingest", Visibility::Only(&[Project::Nidhogg])),
    ("inspect", Visibility::Only(&[Project::Pbfhogg])),
    ("invalidate", Visibility::Except(MEASURED_DB_ABSENT)),
    ("kill", Visibility::Any),
    ("lint-corpus", Visibility::Only(&[Project::Piners])),
    ("lint-results", Visibility::Only(&[Project::Piners])),
    ("list", Visibility::Only(&[Project::Litehtml, Project::Sluggrs])),
    ("lock", Visibility::Any),
    ("merge", Visibility::Only(&[Project::Pbfhogg])),
    ("merge-changes", Visibility::Only(&[Project::Pbfhogg])),
    ("mock-serve", Visibility::Only(&[Project::Ratatoskr])),
    ("multi-extract", Visibility::Only(&[Project::Pbfhogg])),
    ("nid-ingest", Visibility::Only(&[Project::Nidhogg])),
    ("node-store", Visibility::Only(&[Project::Elivagar])),
    ("outline", Visibility::Only(&[Project::Litehtml])),
    ("passthrough", Visibility::Any),
    ("planetiler", Visibility::Only(&[Project::Elivagar])),
    ("pmtiles-inspect", Visibility::Only(&[Project::Elivagar])),
    // PMTiles archives exist only in the two projects that produce or serve
    // them: `src/pmtiles.rs`'s callers are all under `src/elivagar/` and
    // `src/nidhogg/`.
    (
        "pmtiles-stats",
        Visibility::Only(&[Project::Elivagar, Project::Nidhogg]),
    ),
    ("pmtiles-writer", Visibility::Only(&[Project::Elivagar])),
    ("prepare", Visibility::Only(&[Project::Litehtml])),
    ("query", Visibility::Only(&[Project::Nidhogg])),
    ("read", Visibility::Only(&[Project::Pbfhogg])),
    ("regress", Visibility::Only(&[Project::Elivagar])),
    ("renumber", Visibility::Only(&[Project::Pbfhogg])),
    ("repack", Visibility::Only(&[Project::Pbfhogg])),
    ("report", Visibility::Only(&[Project::Litehtml, Project::Sluggrs])),
    ("results", Visibility::Except(MEASURED_DB_ABSENT)),
    ("run", Visibility::Any),
    ("serve", Visibility::Only(&[Project::Nidhogg])),
    ("service-list", Visibility::Only(&[Project::Ratatoskr])),
    ("service-suite", Visibility::Only(&[Project::Ratatoskr])),
    ("service-test", Visibility::Only(&[Project::Ratatoskr])),
    ("sidecar", Visibility::Except(MEASURED_DB_ABSENT)),
    ("sort", Visibility::Only(&[Project::Pbfhogg])),
    ("status", Visibility::Only(&[Project::Nidhogg])),
    ("stop", Visibility::Only(&[Project::Nidhogg])),
    (
        "suite",
        Visibility::Only(&[Project::Pbfhogg, Project::Elivagar, Project::Nidhogg]),
    ),
    ("svg", Visibility::Only(&[Project::Elivagar])),
    ("sync-bench", Visibility::Only(&[Project::Ratatoskr])),
    ("sync-list", Visibility::Only(&[Project::Ratatoskr])),
    ("sync-smoke", Visibility::Only(&[Project::Ratatoskr])),
    ("tags-filter", Visibility::Only(&[Project::Pbfhogg])),
    // Refused in litehtml and sluggrs at `bootstrap.rs`'s dispatch; available
    // everywhere else, including unrecognised `Other(_)` trees.
    (
        "test",
        Visibility::Except(&[Project::Litehtml, Project::Sluggrs]),
    ),
    ("tilegen", Visibility::Only(&[Project::Elivagar])),
    ("tilemaker", Visibility::Only(&[Project::Elivagar])),
    ("tiles", Visibility::Only(&[Project::Nidhogg])),
    ("time-filter", Visibility::Only(&[Project::Pbfhogg])),
    ("update", Visibility::Only(&[Project::Nidhogg])),
    (
        "verify",
        Visibility::Only(&[Project::Pbfhogg, Project::Elivagar, Project::Nidhogg]),
    ),
    ("visual", Visibility::Only(&[Project::Litehtml, Project::Sluggrs])),
    ("visual-status", Visibility::Only(&[Project::Litehtml, Project::Sluggrs])),
    ("wc", Visibility::Any),
    ("write", Visibility::Only(&[Project::Pbfhogg])),
];

/// Whether `name` should be visible in `project`. Unknown names are visible
/// (fail open: a subcommand missing from the table must never vanish).
///
/// Matching is on the enum variant, not on `Project`'s `PartialEq`, so a
/// foreign project declared as `project = "piners"` in a *non*-piners
/// checkout - which detects as `Project::Other("piners")` - never satisfies
/// an `Only(&[Project::Piners])` entry. No table entry names `Other`, so the
/// coarseness of discriminant comparison for that variant is unobservable.
pub(crate) fn visible_in(name: &str, project: Project) -> bool {
    let Some((_, vis)) = TABLE.iter().find(|(n, _)| *n == name) else {
        return true;
    };
    match vis {
        Visibility::Any => true,
        Visibility::Only(projects) => same_variant(projects, project),
        Visibility::Except(projects) => !same_variant(projects, project),
    }
}

/// Whether `project` is one of `projects`, compared by enum variant rather
/// than by `PartialEq`, so an `Other(_)` payload never matters. No table entry
/// names `Other`, so that coarseness is unobservable.
fn same_variant(projects: &[Project], project: Project) -> bool {
    projects
        .iter()
        .any(|p| std::mem::discriminant(p) == std::mem::discriminant(&project))
}
