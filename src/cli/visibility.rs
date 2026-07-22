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
}

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
    ("invalidate", Visibility::Any),
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
    ("pmtiles-stats", Visibility::Any),
    ("pmtiles-writer", Visibility::Only(&[Project::Elivagar])),
    ("prepare", Visibility::Only(&[Project::Litehtml])),
    ("query", Visibility::Only(&[Project::Nidhogg])),
    ("read", Visibility::Only(&[Project::Pbfhogg])),
    ("regress", Visibility::Only(&[Project::Elivagar])),
    ("renumber", Visibility::Only(&[Project::Pbfhogg])),
    ("repack", Visibility::Only(&[Project::Pbfhogg])),
    ("report", Visibility::Only(&[Project::Litehtml, Project::Sluggrs])),
    ("results", Visibility::Any),
    ("run", Visibility::Any),
    ("serve", Visibility::Only(&[Project::Nidhogg])),
    ("service-list", Visibility::Only(&[Project::Ratatoskr])),
    ("service-suite", Visibility::Only(&[Project::Ratatoskr])),
    ("service-test", Visibility::Only(&[Project::Ratatoskr])),
    ("sidecar", Visibility::Any),
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
    ("test", Visibility::Any),
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
        Visibility::Only(projects) => projects
            .iter()
            .any(|p| std::mem::discriminant(p) == std::mem::discriminant(&project)),
    }
}
