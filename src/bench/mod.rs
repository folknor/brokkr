//! Script benches: `[bench.NAME]` tables holding the Lua that runs a bench.
//!
//! The schema is read at load (`crate::config::bench`). Here: the static check
//! `brokkr check`'s `bench` phase runs (every bench's script read and
//! compiled, nothing executed), the bench store with brokkr's adapter to
//! it, `brokkr results`' reading of that store (`results`), and the handles
//! a script's paths reach its children through (`resources`). Writing the
//! store and running an iteration wait for the script-bench runner. The
//! store's wire contract (`store::wire`) is served by the
//! `brokkr-results-server` binary; brokkr's client of it is still to come.

// An expectation goes unfulfilled, and so fails the build, once the runner
// uses everything it covers: then it is removed.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "no command writes the bench store yet")
)]
mod evidence;
mod lua;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "no command runs a script bench yet")
)]
mod resources;
pub(crate) mod results;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        unused_imports,
        reason = "no command writes the bench store yet"
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
