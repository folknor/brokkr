//! Script benches: `[bench.NAME]` tables holding the Lua that runs a bench.
//!
//! The schema is read at load (`crate::config::bench`). Here: the static check
//! `brokkr check`'s `bench` phase runs (every bench's script read and
//! compiled, nothing executed), the runner `brokkr bench NAME` drives
//! (`runner`), the dellingr seam its scripts run through (`lua`), the bench
//! store with brokkr's adapter to it (`store`, `evidence`), `brokkr results`'
//! reading of that store (`results`), and the handles a script's paths reach
//! its children through (`resources`). The store's wire contract
//! (`store::wire`) is served by the `brokkr-results-server` binary, and
//! `remote` is brokkr's client of it, used in place of the local store when
//! the user-wide config names a server; `publication` is the retry and
//! uncertainty latch both share.

mod evidence;
mod lua;
mod publication;
mod remote;
mod resources;
pub(crate) mod results;
pub(crate) mod runner;
// The server's side of the wire (`wire::execute`, `wire::serve`) and the
// readers of a whole stored document and of the transaction state are the
// server's alone: an expectation goes unfulfilled, and so fails the build,
// once brokkr uses them all.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the store's server-side entry points are used by brokkr-results-server only"
    )
)]
pub(crate) mod store;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod wire_tests;

use crate::config::bench::{Bench, BenchScript};
use std::path::PathBuf;

/// Why one bench's script failed the static check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScriptFinding {
    pub(crate) bench: String,
    /// The file the reader should open: the `.lua` file, or the config file
    /// holding an inline script.
    pub(crate) file: PathBuf,
    pub(crate) problem: String,
}

impl std::fmt::Display for ScriptFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[bench.{}] {}: {}",
            self.bench,
            self.file.display(),
            self.problem
        )
    }
}

/// Read and compile one bench's script. An inline script's line numbers
/// count from the first line of the `script` string, not of its config file.
pub(crate) fn check_script(bench: &Bench) -> Result<(), ScriptFinding> {
    let finding = |file: PathBuf, problem: String| ScriptFinding {
        bench: bench.name.clone(),
        file,
        problem,
    };
    match &bench.script {
        BenchScript::Inline {
            source,
            declared_in,
        } => lua::compile(source.as_bytes())
            .map_err(|e| finding(declared_in.clone(), format!("inline script, {e}"))),
        BenchScript::File { path, declared_in } => {
            let source = std::fs::read(path).map_err(|e| {
                finding(
                    path.clone(),
                    format!(
                        "script_file (set in {}) cannot be read: {e}",
                        declared_in.display()
                    ),
                )
            })?;
            lua::compile(&source).map_err(|e| finding(path.clone(), e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::collections::BTreeMap;

    fn bench(script: BenchScript) -> Bench {
        Bench {
            name: "cat".into(),
            script,
            target: None,
            profile: None,
            features: Vec::new(),
            default_features: true,
            uses: Vec::new(),
            params: BTreeMap::new(),
        }
    }

    #[test]
    fn an_inline_failure_names_the_config_file_and_the_script_line() {
        let finding = check_script(&bench(BenchScript::Inline {
            source: "x = 1\nfunction bench(\n".into(),
            declared_in: PathBuf::from("brokkr.toml"),
        }))
        .unwrap_err();
        let shown = finding.to_string();
        assert!(
            shown.starts_with("[bench.cat] brokkr.toml: inline script, line "),
            "{shown}"
        );
    }

    #[test]
    fn a_missing_script_file_is_a_finding_naming_where_it_was_set() {
        let dir = crate::test_scratch::scratch("bench_check", "missing");
        let finding = check_script(&bench(BenchScript::File {
            path: dir.join("cat.lua"),
            declared_in: dir.join("benches.toml"),
        }))
        .unwrap_err();
        assert_eq!(finding.file, dir.join("cat.lua"));
        assert!(
            finding.problem.contains("benches.toml"),
            "{}",
            finding.problem
        );

        std::fs::write(dir.join("ok.lua"), "function bench() end\n").unwrap();
        check_script(&bench(BenchScript::File {
            path: dir.join("ok.lua"),
            declared_in: dir.join("benches.toml"),
        }))
        .unwrap();
    }
}
