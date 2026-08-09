//! Workload registry resolution: a name -> a verified, frozen invocation.

use crate::config::{DevConfig, MogwaiConfig, MogwaiWorkload};
use crate::error::DevError;

/// A workload resolved from the registry, ready to run.
#[derive(Debug)]
pub(crate) struct Resolved<'a> {
    /// The registered name, and the label the run is filed under in
    /// `results.db`.
    pub(crate) name: String,
    /// The frozen entry, borrowed from the parsed config.
    pub(crate) entry: &'a MogwaiWorkload,
}

impl Resolved<'_> {
    /// The argv passed to the mogwai binary, as borrowed `&str`s.
    pub(crate) fn args(&self) -> Vec<&str> {
        self.entry.args.iter().map(String::as_str).collect()
    }
}

/// Fetch the `[mogwai]` block, with an actionable error when it's absent.
pub(crate) fn config(dev_config: &DevConfig) -> Result<&MogwaiConfig, DevError> {
    dev_config.mogwai.as_ref().ok_or_else(|| {
        DevError::Config(
            "no [mogwai] section in brokkr.toml - `brokkr mogwai` needs the CLI \
             package and at least one frozen workload:\n\n  [mogwai]\n  \
             package = \"mogwai-cli\"\n\n  [mogwai.workloads.screen-probe]\n  \
             description = \"...\"\n  args = [\"screen\", \"--preset\", \"...\"]\n  \
             runs = 3"
                .into(),
        )
    })
}

/// Resolve a workload name against the registry.
///
/// Retired entries (those carrying a `successor`) refuse to run. The refusal is
/// the whole point of recording the pointer: the alternative is a name that
/// still works but no longer measures what its historical rows measured, which
/// is the one failure this registry exists to prevent.
pub(crate) fn resolve<'a>(
    dev_config: &'a DevConfig,
    name: &str,
) -> Result<Resolved<'a>, DevError> {
    let cfg = config(dev_config)?;
    let entry = cfg.workload(name)?;

    if let Some(successor) = &entry.successor {
        return Err(DevError::Config(format!(
            "workload {name:?} is retired - its invocation changed, and it was \
             replaced by {successor:?}.\n  its rows are still queryable \
             (`brokkr results --command {name}`), but a new run under this name \
             would not be comparable to them.\n  run the successor instead: \
             `brokkr mogwai {successor}`"
        )));
    }

    Ok(Resolved {
        name: name.to_owned(),
        entry,
    })
}

/// Render the registry as the bare `brokkr mogwai` index.
///
/// Retired entries are listed too, marked with their successor - a name that
/// appears in months-old notes should be findable here rather than look like it
/// was deleted.
pub(crate) fn format_index(cfg: &MogwaiConfig) -> String {
    if cfg.workloads.is_empty() {
        return "no workloads registered - add [mogwai.workloads.<name>] to brokkr.toml"
            .to_owned();
    }

    let width = cfg.workloads.keys().map(String::len).max().unwrap_or(0);
    let mut out = String::new();
    for (name, workload) in &cfg.workloads {
        let detail = match (&workload.successor, &workload.description) {
            (Some(successor), _) => format!("retired -> {successor}"),
            (None, Some(description)) => description.clone(),
            (None, None) => String::new(),
        };
        // The cost of a baseline refresh, legible before it is paid: N runs of
        // a month-long walk is an evening, not a coffee break.
        let cost = match (workload.runs, workload.expect_seconds) {
            (Some(n), Some(s)) => format!(" [{n} run(s), ~{s}s each]"),
            (Some(n), None) => format!(" [{n} run(s)]"),
            (None, Some(s)) => format!(" [~{s}s each]"),
            (None, None) => String::new(),
        };
        out.push_str(&format!("  {name:<width$}  {detail}{cost}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A fresh scratch dir under the crate's gitignored `target/`
    /// (project rules forbid `/tmp`).
    fn tmpdir(test_name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/mogwai")
            .join(test_name);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_with(toml_src: &str) -> DevConfig {
        let dir = tmpdir("cfg");
        fs::write(dir.join("brokkr.toml"), toml_src).unwrap();
        crate::config::load(&dir).unwrap().1
    }

    const BASE: &str = "project = \"mogwai\"\n\n[mogwai]\npackage = \"mogwai-cli\"\n\n";

    #[test]
    fn resolves_a_registered_workload() {
        let cfg = config_with(&format!(
            "{BASE}[mogwai.workloads.screen-probe]\nargs = [\"screen\", \"--probe\"]\n"
        ));
        let resolved = resolve(&cfg, "screen-probe").unwrap();
        assert_eq!(resolved.name, "screen-probe");
        assert_eq!(resolved.args(), vec!["screen", "--probe"]);
    }

    #[test]
    fn retired_workload_refuses_and_names_its_successor() {
        let cfg = config_with(&format!(
            "{BASE}[mogwai.workloads.screen-v1]\nargs = [\"screen\"]\n\
             successor = \"screen-v2\"\n\n\
             [mogwai.workloads.screen-v2]\nargs = [\"screen\", \"--wide\"]\n"
        ));
        let err = resolve(&cfg, "screen-v1").unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        // The refusal must point at the heir and say the old rows survive,
        // otherwise the pointer buys nothing over deleting the entry.
        assert!(msg.contains("screen-v2"), "{msg}");
        assert!(msg.contains("brokkr results"), "{msg}");
    }

    #[test]
    fn unknown_workload_lists_only_the_live_names() {
        let cfg = config_with(&format!(
            "{BASE}[mogwai.workloads.alpha]\nargs = [\"a\"]\n\n\
             [mogwai.workloads.beta]\nargs = [\"b\"]\nsuccessor = \"alpha\"\n"
        ));
        let err = resolve(&cfg, "gamma").unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("alpha"), "{msg}");
        // Suggesting a retired name would send the reader to a second refusal.
        assert!(!msg.contains("beta"), "{msg}");
    }

    #[test]
    fn measurement_flags_are_rejected_at_parse_time() {
        let dir = tmpdir("mode_flag");
        fs::write(
            dir.join("brokkr.toml"),
            format!("{BASE}[mogwai.workloads.w]\nargs = [\"screen\", \"--bench\"]\n"),
        )
        .unwrap();
        let err = crate::config::load(&dir).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("--bench"), "{msg}");
    }

    #[test]
    fn successor_pointing_nowhere_is_rejected() {
        let dir = tmpdir("dangling");
        fs::write(
            dir.join("brokkr.toml"),
            format!("{BASE}[mogwai.workloads.w]\nargs = [\"a\"]\nsuccessor = \"ghost\"\n"),
        )
        .unwrap();
        let err = crate::config::load(&dir).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("ghost"), "{msg}");
    }

    #[test]
    fn empty_args_are_rejected() {
        let dir = tmpdir("empty_args");
        fs::write(
            dir.join("brokkr.toml"),
            format!("{BASE}[mogwai.workloads.w]\nargs = []\n"),
        )
        .unwrap();
        let err = crate::config::load(&dir).unwrap_err();
        let DevError::Config(msg) = err else {
            panic!("expected DevError::Config, got {err:?}");
        };
        assert!(msg.contains("stated in full"), "{msg}");
    }

    #[test]
    fn index_marks_retired_entries() {
        let cfg = config_with(&format!(
            "{BASE}[mogwai.workloads.alpha]\nargs = [\"a\"]\n\
             description = \"the live one\"\n\n\
             [mogwai.workloads.beta]\nargs = [\"b\"]\nsuccessor = \"alpha\"\n"
        ));
        let index = format_index(config(&cfg).unwrap());
        assert!(index.contains("the live one"), "{index}");
        assert!(index.contains("retired -> alpha"), "{index}");
    }
}
