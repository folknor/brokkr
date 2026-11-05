//! Script benches: `[bench.NAME]` tables holding the Lua that runs a bench.
//!
//! The schema is read at load (`crate::config::bench`). Here: the static check
//! `brokkr check`'s `bench` phase runs (every bench's script read and
//! compiled, nothing executed), the runner `brokkr bench NAME` drives
//! (`runner`), the dellingr seam its scripts run through (`lua`), the bench
//! store with brokkr's adapter to it (`store`, `evidence`), `brokkr results`'
//! reading of that store (`results`), the resolved config a script sees
//! (`view`), the params bound against it (`params`), and the handles a
//! script's paths reach its children through (`resources`). The store's wire contract
//! (`store::wire`) is served by the `brokkr-results-server` binary, and
//! `remote` is brokkr's client of it, used in place of the local store when
//! the user-wide config names a server; `publication` is the retry and
//! uncertainty latch both share.

pub(crate) mod evidence;
mod json;
mod lua;
mod params;
mod publication;
mod remote;
mod resources;
pub(crate) mod results;
pub(crate) mod runner;
mod script_lines;
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
mod view;
#[cfg(test)]
mod wire_tests;

use crate::config::bench::{Bench, BenchScript};
use std::path::{Path, PathBuf};

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

/// How an inline script's errors name it: by its config file and that file's
/// lines when they can be mapped (`script_lines`), else as the bench's
/// script, its lines counted from the string's first, with why not.
pub(crate) struct InlineLines {
    pub(crate) chunk: String,
    pub(crate) lines: Option<script_lines::LineMap>,
    why_not: Option<String>,
}

impl InlineLines {
    pub(crate) fn of(bench: &str, source: &str, declared_in: &Path) -> Self {
        match script_lines::locate(declared_in, bench, source) {
            Ok(lines) => Self {
                chunk: declared_in.display().to_string(),
                lines: Some(lines),
                why_not: None,
            },
            Err(why) => Self {
                chunk: format!("[bench.{bench}] script"),
                lines: None,
                why_not: Some(why),
            },
        }
    }

    /// Why the lines are the string's own, when they are.
    pub(crate) fn why_not(&self) -> Option<&str> {
        self.why_not.as_deref()
    }

    /// A compile error as a finding's problem, after the config file's name.
    pub(crate) fn problem(&self, error: &lua::CompileError) -> String {
        match &self.why_not {
            None => error.to_string(),
            Some(why) => {
                format!("inline script, {error} (lines count from the script string, since {why})")
            }
        }
    }
}

/// Read and compile one bench's script. An inline script's line numbers are
/// its config file's when they can be mapped, else the string's own.
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
        } => {
            let lines = InlineLines::of(&bench.name, source, declared_in);
            lua::compile(source.as_bytes(), lines.lines.as_ref())
                .map_err(|e| finding(declared_in.clone(), lines.problem(&e)))
        }
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
            lua::compile(&source, None).map_err(|e| finding(path.clone(), e.to_string()))
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
            hotpath_features: Vec::new(),
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

    /// When the declaring file still holds the script, the finding names the
    /// file's own line; once it no longer does, the string's, saying why.
    #[test]
    fn an_inline_failure_names_the_config_files_line_when_it_can() {
        let dir = crate::test_scratch::scratch("bench_check", "mapped");
        let file = dir.join("brokkr.toml");
        let source = "x = 1\nfunction bench(\n";
        std::fs::write(
            &file,
            format!("project = \"x\"\n\n[bench.cat]\nscript = \"\"\"\n{source}\"\"\"\n"),
        )
        .unwrap();
        let inline = BenchScript::Inline {
            source: source.into(),
            declared_in: file.clone(),
        };
        let shown = check_script(&bench(inline.clone()))
            .unwrap_err()
            .to_string();
        assert!(
            shown.starts_with(&format!("[bench.cat] {}: line 7: ", file.display())),
            "{shown}"
        );
        std::fs::write(&file, "[bench.cat]\nscript = \"edited\"\n").unwrap();
        let shown = check_script(&bench(inline)).unwrap_err().to_string();
        assert!(shown.contains("inline script, line 3: "), "{shown}");
        assert!(shown.contains("has changed since it was loaded"), "{shown}");
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
