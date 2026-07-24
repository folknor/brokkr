//! `brokkr man [TOPIC]` - the bundled documentation.
//!
//! The `docs/**.md` files are compiled into the binary with `include_str!` and
//! rendered to the terminal through the markdown->ANSI renderer in [`render`].
//! With no topic, list what is available; with a topic, render it (colour
//! auto-disabled when stdout is not a TTY or `NO_COLOR` is set).
//!
//! Topics are filtered by the detected project, the same way `--help` filters
//! subcommands: a pbfhogg tree has no use for the piners corpus reference. The
//! project-agnostic topics - the ones about `check`, `deps`, `clippy`,
//! measurement and config - are always listed, because they describe machinery
//! that runs everywhere.
//!
//! CLAUDE.md carries a parallel list of these files annotated "**read when**
//! …". That list is prose and nothing verifies it; [`TOPICS`] is the executable
//! version, and `include_str!` means a renamed or deleted doc breaks the build
//! rather than rotting silently.

mod render;

use std::io::{IsTerminal, Write};

use crate::cli::Visibility;
use crate::project::Project;

/// One bundled doc: the name typed on the command line, a one-line summary for
/// the listing, the compiled-in markdown, and which projects it applies to.
struct Topic {
    name: &'static str,
    summary: &'static str,
    content: &'static str,
    visibility: Visibility,
}

/// Every bundled doc, in listing order: the project-agnostic ones first, then
/// the project-scoped ones.
///
/// `include_str!` paths are relative to this file (`src/`), so they climb one
/// level to the repo root.
const TOPICS: &[Topic] = &[
    Topic {
        name: "config",
        summary: "brokkr.toml: host sections, [[check]] sweeps, [test] profiles",
        content: include_str!("../docs/brokkr.toml.md"),
        visibility: Visibility::Any,
    },
    Topic {
        name: "check",
        summary: "the check/test validation pipeline, sweeps, profiles, the gate",
        content: include_str!("../docs/commands/check.md"),
        visibility: Visibility::Any,
    },
    Topic {
        name: "clippy",
        summary: "the investigative single-phase clippy runner",
        content: include_str!("../docs/commands/clippy.md"),
        visibility: Visibility::Any,
    },
    Topic {
        name: "deps",
        summary: "the dependency audit: duplicate versions, git/path deps, staleness",
        content: include_str!("../docs/commands/deps.md"),
        visibility: Visibility::Any,
    },
    Topic {
        name: "measure",
        summary: "--bench/--hotpath/--alloc, the sidecar profiler, marker FIFOs",
        content: include_str!("../docs/commands/measure.md"),
        visibility: Visibility::Any,
    },
    Topic {
        name: "output-channels",
        summary: "where a run's output goes: stdout, stderr key=value, FIFO markers",
        content: include_str!("../docs/commands/output-channels.md"),
        visibility: Visibility::Any,
    },
    Topic {
        name: "datasets",
        summary: "[<host>.datasets.*] pbf/osc/pmtiles entries and variant selection",
        content: include_str!("../docs/brokkr.toml.datasets.md"),
        visibility: Visibility::Only(&[Project::Pbfhogg, Project::Elivagar, Project::Nidhogg]),
    },
    Topic {
        name: "pbfhogg",
        summary: "pbfhogg commands, verify subcommands, snapshot graph, OSC parser",
        content: include_str!("../docs/projects/pbfhogg.md"),
        visibility: Visibility::Only(&[Project::Pbfhogg]),
    },
    Topic {
        name: "elivagar",
        summary: "elivagar commands, regress, the pmtiles corpus, the durable tilegen output store",
        content: include_str!("../docs/projects/elivagar.md"),
        visibility: Visibility::Only(&[Project::Elivagar]),
    },
    Topic {
        name: "dispatch-differences",
        summary: "how pbfhogg and elivagar's dispatch layers differ",
        content: include_str!("../docs/projects/pbfhogg-vs-elivagar.md"),
        visibility: Visibility::Only(&[Project::Pbfhogg, Project::Elivagar]),
    },
    Topic {
        name: "nidhogg",
        summary: "nidhogg commands, server lifecycle, the API client",
        content: include_str!("../docs/projects/nidhogg.md"),
        visibility: Visibility::Only(&[Project::Nidhogg]),
    },
    Topic {
        name: "visual",
        summary: "visual reference testing: visual, list, approve, report, prepare",
        content: include_str!("../docs/commands/visual.md"),
        visibility: Visibility::Only(&[Project::Litehtml, Project::Sluggrs]),
    },
    Topic {
        name: "litehtml",
        summary: "litehtml/sluggrs internals: modules, fixtures, Node.js scripts",
        content: include_str!("../docs/projects/litehtml.md"),
        visibility: Visibility::Only(&[Project::Litehtml, Project::Sluggrs]),
    },
    Topic {
        name: "ratatoskr",
        summary: "the harness model, saehrimnir contract, lua runtime, artefacts",
        content: include_str!("../docs/projects/ratatoskr.md"),
        visibility: Visibility::Only(&[Project::Ratatoskr]),
    },
    Topic {
        name: "sync",
        summary: "mock-serve, sync-list, sync-smoke, sync-bench",
        content: include_str!("../docs/commands/sync.md"),
        visibility: Visibility::Only(&[Project::Ratatoskr]),
    },
    Topic {
        name: "gate",
        summary: "--gate/--as-baseline, baseline pinning, sync-bench thresholds",
        content: include_str!("../docs/commands/ratatoskr-gate.md"),
        visibility: Visibility::Only(&[Project::Ratatoskr]),
    },
    Topic {
        name: "service",
        summary: "service-test, service-suite, service-list: lua VM, frontmatter",
        content: include_str!("../docs/commands/service.md"),
        visibility: Visibility::Only(&[Project::Ratatoskr]),
    },
    Topic {
        name: "piners",
        summary: "harness NDJSON/manifest contracts, runs.db, corpus-results",
        content: include_str!("../docs/projects/piners.md"),
        visibility: Visibility::Only(&[Project::Piners]),
    },
    Topic {
        name: "piners-config",
        summary: "the [piners] config block: corpus_root, registry_dir, feeds",
        content: include_str!("../docs/brokkr.toml.piners.md"),
        visibility: Visibility::Only(&[Project::Piners]),
    },
    Topic {
        name: "corpus",
        summary: "the parity-corpus runner: pins.toml, probes, dispositions",
        content: include_str!("../docs/commands/corpus.md"),
        visibility: Visibility::Only(&[Project::Piners]),
    },
    Topic {
        name: "lint-corpus",
        summary: "the differential-lint corpus: lints.toml, diffs, --reanchor",
        content: include_str!("../docs/commands/lint-corpus.md"),
        visibility: Visibility::Only(&[Project::Piners]),
    },
];

/// The topics available in `project`, in table order.
fn visible(project: Project) -> Vec<&'static Topic> {
    TOPICS
        .iter()
        .filter(|t| crate::cli::visible_to(&t.visibility, project))
        .collect()
}

/// Colour is on only when stdout is a real terminal and `NO_COLOR` is unset.
fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some() || !std::io::stdout().is_terminal()
}

/// Render `topic` to the terminal, or list the available topics when `None`.
///
/// An unknown topic is an error rather than a silent listing, and one that
/// exists but belongs to another project says so - the same courtesy
/// `project::require()` extends to a wrong-project command, rather than
/// pretending the doc does not exist.
pub fn run(topic: Option<&str>, project: Project) -> Result<(), crate::error::DevError> {
    let Some(name) = topic else {
        write_stdout(&list_topics(project));
        return Ok(());
    };

    let Some(found) = TOPICS.iter().find(|t| t.name == name) else {
        return Err(crate::error::DevError::Config(format!(
            "unknown man topic '{name}'. Run `brokkr man` to list the topics \
             available in this project."
        )));
    };

    if !crate::cli::visible_to(&found.visibility, project) {
        return Err(crate::error::DevError::Config(format!(
            "man topic '{name}' documents another project's surface (current: \
             {project}). Run `brokkr man` to list this project's topics."
        )));
    }

    write_stdout(&render::render(found.content, no_color()));
    Ok(())
}

/// Write to stdout, treating a closed downstream pipe (EPIPE, e.g. `| less`
/// quit early) as a clean exit; any other write error is reported but left
/// non-fatal for a one-shot doc print.
fn write_stdout(text: &str) {
    if let Err(e) = std::io::stdout().write_all(text.as_bytes())
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        eprintln!("brokkr man: write error: {e}");
    }
}

/// The bare-`brokkr man` listing: each topic name padded to a common width
/// followed by its one-line summary.
fn list_topics(project: Project) -> String {
    let topics = visible(project);
    let width = topics.iter().map(|t| t.name.len()).max().unwrap_or(0);
    // An undetectable project is `Other("")`, whose name is empty; naming it
    // would leave a dangling space ("Bundled docs for . Run ..."). Drop the
    // "for X" clause entirely in that case - only the generic topics show.
    let mut out = if project.name().is_empty() {
        "Bundled docs. Run `brokkr man <topic>` to read one.\n\n".to_string()
    } else {
        format!("Bundled docs for {project}. Run `brokkr man <topic>` to read one.\n\n")
    };
    for topic in topics {
        out.push_str(&format!(
            "  {name:width$}  {summary}\n",
            name = topic.name,
            summary = topic.summary
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{TOPICS, list_topics, visible};
    use crate::project::Project;

    /// Topic names are what the user types; a duplicate would make one of them
    /// unreachable, since lookup takes the first match.
    #[test]
    fn topic_names_are_unique() {
        let mut names: Vec<&str> = TOPICS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate topic name");
    }

    /// The project-agnostic docs describe machinery that runs in every tree, so
    /// they must survive the filter everywhere - including an unrecognised
    /// project, which is exactly where someone is most likely to need them.
    #[test]
    fn generic_topics_survive_every_project() {
        for project in [
            Project::Pbfhogg,
            Project::Piners,
            Project::Litehtml,
            Project::Other("some-foreign-repo"),
        ] {
            let names: Vec<&str> = visible(project).iter().map(|t| t.name).collect();
            for expected in ["check", "clippy", "deps", "config", "measure"] {
                assert!(names.contains(&expected), "`{expected}` missing in {project}");
            }
        }
    }

    /// And the project-scoped ones must not leak across.
    #[test]
    fn scoped_topics_stay_in_their_project() {
        let piners: Vec<&str> = visible(Project::Piners).iter().map(|t| t.name).collect();
        assert!(piners.contains(&"corpus"));
        assert!(!piners.contains(&"pbfhogg"));

        let pbfhogg: Vec<&str> = visible(Project::Pbfhogg).iter().map(|t| t.name).collect();
        assert!(pbfhogg.contains(&"pbfhogg"));
        assert!(!pbfhogg.contains(&"corpus"));
    }

    /// An unrecognised project still gets a usable listing rather than an empty
    /// one - the generic topics are the whole point of the `Any` arm.
    #[test]
    fn other_project_listing_is_not_empty() {
        let out = list_topics(Project::Other("some-foreign-repo"));
        assert!(out.contains("check"), "{out}");
        assert!(!out.contains("corpus"), "{out}");
    }

    /// The no-project fallback is `Other("")`, whose empty name must not leave
    /// a dangling space in the header ("Bundled docs for . Run ...").
    #[test]
    fn empty_project_header_has_no_dangling_space() {
        let out = list_topics(Project::Other(""));
        assert!(
            out.starts_with("Bundled docs. Run `brokkr man <topic>`"),
            "clean header without a stray space: {out:?}"
        );
        assert!(!out.contains(" for . "), "no empty 'for X' clause: {out:?}");
        assert!(out.contains("check"), "generic topics still listed: {out}");
    }
}
