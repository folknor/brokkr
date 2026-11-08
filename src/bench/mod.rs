//! Script benches: `[bench.NAME]` tables holding the Lua that runs a bench.
//!
//! The schema is read at load (`crate::config::bench`). Here: the static check
//! `brokkr check`'s `bench` phase runs (every bench's script read and
//! compiled, nothing executed), the runner `brokkr bench NAME` drives
//! (`runner`), the dellingr seam its scripts run through (`lua`), the bench
//! store with brokkr's adapter to it (`store`, `evidence`), `brokkr results`'
//! reading of that store (`results`), the resolved config a script sees
//! (`view`), the params bound against it (`params`), and the handles a
//! script's paths reach its children through (`resources`), and the external
//! programs a call may run in place of the built one (`external`). The store's wire contract
//! (`store::wire`) is served by the `brokkr-results-server` binary, and
//! `remote` is brokkr's client of it, used in place of the local store when
//! the user-wide config names a server; `publication` is the retry and
//! uncertainty latch both share.

pub(crate) mod evidence;
mod external;
mod json;
mod library;
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
        // A library module's finding belongs to no one bench.
        if self.bench.is_empty() {
            return write!(f, "bench_lib {}: {}", self.file.display(), self.problem);
        }
        write!(
            f,
            "[bench.{}] {}: {}",
            self.bench,
            self.file.display(),
            self.problem
        )
    }
}

/// Read the `bench_lib` directory `dir` and check every module in it: it
/// compiles, defines no global, reads no `_G` and reads no name beyond the
/// builtins and dellingr's library (`lua::check_module`). Runs none of it.
pub(crate) fn check_library(dir: &Path) -> Vec<ScriptFinding> {
    let finding = |file: &Path, problem: String| ScriptFinding {
        bench: String::new(),
        file: file.to_path_buf(),
        problem,
    };
    let library = match library::Library::read(dir) {
        Ok(library) => library,
        Err(error) => return vec![finding(dir, error.to_string())],
    };
    let mut findings = Vec::new();
    for module in library.modules.values() {
        let checked = match &module.compiled {
            Err(error) => Err(error.clone()),
            Ok(_) => lua::check_module(&module.source, &module.path.display().to_string()),
        };
        match checked {
            Err(error) => findings.push(finding(&module.path, error.to_string())),
            Ok(problems) => findings.extend(
                problems
                    .iter()
                    .map(|problem| finding(&module.path, problem.to_string())),
            ),
        }
    }
    findings
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

/// What the static check found in one bench's script.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ScriptCheck {
    /// Every problem: a compile failure alone, or every unresolved name.
    pub(crate) findings: Vec<ScriptFinding>,
    /// Set when the script reads `_G`, so its names went unchecked: why, as
    /// a line for the operator.
    pub(crate) unchecked: Option<String>,
}

/// Read and compile one bench's script and resolve its global names
/// (`lua::check`). An inline script's line numbers are its config file's
/// when they can be mapped, else the string's own.
pub(crate) fn check_script(bench: &Bench) -> ScriptCheck {
    let finding = |file: &Path, problem: String| ScriptFinding {
        bench: bench.name.clone(),
        file: file.to_path_buf(),
        problem,
    };
    let (file, checked, inline): (&Path, _, Option<InlineLines>) = match &bench.script {
        BenchScript::Inline {
            source,
            declared_in,
        } => {
            let lines = InlineLines::of(&bench.name, source, declared_in);
            let checked = lua::check(source.as_bytes(), lines.lines.as_ref());
            (declared_in, checked, Some(lines))
        }
        BenchScript::File { path, declared_in } => match std::fs::read(path) {
            Ok(source) => (path, lua::check(&source, None), None),
            Err(e) => {
                return ScriptCheck {
                    findings: vec![finding(
                        path,
                        format!(
                            "script_file (set in {}) cannot be read: {e}",
                            declared_in.display()
                        ),
                    )],
                    unchecked: None,
                };
            }
        },
    };
    let describe = |e: &lua::CompileError| match &inline {
        Some(lines) => lines.problem(e),
        None => e.to_string(),
    };
    match checked {
        Err(error) => ScriptCheck {
            findings: vec![finding(file, describe(&error))],
            unchecked: None,
        },
        Ok(names) => ScriptCheck {
            findings: names
                .problems
                .iter()
                .map(|problem| finding(file, describe(problem)))
                .collect(),
            // Worded as a finding is, so an inline script's line says whose
            // it is the same way.
            unchecked: names.unchecked_at.map(|line| {
                let why = lua::CompileError {
                    line,
                    message: "reads _G, so its global names are not checked: a script \
                              holding the global table can define any name"
                        .to_owned(),
                };
                finding(file, describe(&why)).to_string()
            }),
        },
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
            library: None,
            annotations: crate::config::bench_annotations::Annotations::NONE,
        }
    }

    /// The one finding a script that should fail exactly once produces.
    fn only(checked: ScriptCheck) -> ScriptFinding {
        assert_eq!(checked.findings.len(), 1, "{:?}", checked.findings);
        checked.findings.into_iter().next().unwrap()
    }

    fn inline(source: &str) -> ScriptCheck {
        check_script(&bench(BenchScript::Inline {
            source: source.into(),
            declared_in: PathBuf::from("brokkr.toml"),
        }))
    }

    /// Every module of the library is checked, each finding naming the
    /// module's file under `bench_lib`, not a bench; a clean one adds none.
    #[test]
    fn the_library_check_names_each_modules_file() {
        let dir = crate::test_scratch::scratch("bench_check", "library");
        std::fs::write(dir.join("ok.lua"), "local M = {}\nreturn M\n").unwrap();
        std::fs::write(dir.join("leaky.lua"), "helper = 1\nreturn {}\n").unwrap();
        std::fs::write(dir.join("broken.lua"), "return (\n").unwrap();
        let findings = check_library(&dir);
        let shown: Vec<String> = findings.iter().map(ToString::to_string).collect();
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert!(
            shown[0].starts_with("bench_lib ") && shown[0].contains("broken.lua"),
            "{shown:?}"
        );
        assert!(
            shown[1].contains("leaky.lua: line 1: assigns the global helper"),
            "{shown:?}"
        );
        assert_eq!(check_library(&dir.join("absent")).len(), 1);
    }

    #[test]
    fn an_inline_failure_names_the_config_file_and_the_script_line() {
        let finding = only(inline("x = 1\nfunction bench(\n"));
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
        let shown = only(check_script(&bench(inline.clone()))).to_string();
        assert!(
            shown.starts_with(&format!("[bench.cat] {}: line 7: ", file.display())),
            "{shown}"
        );
        std::fs::write(&file, "[bench.cat]\nscript = \"edited\"\n").unwrap();
        let shown = only(check_script(&bench(inline))).to_string();
        assert!(shown.contains("inline script, line 3: "), "{shown}");
        assert!(shown.contains("has changed since it was loaded"), "{shown}");
    }

    #[test]
    fn a_missing_script_file_is_a_finding_naming_where_it_was_set() {
        let dir = crate::test_scratch::scratch("bench_check", "missing");
        let finding = only(check_script(&bench(BenchScript::File {
            path: dir.join("cat.lua"),
            declared_in: dir.join("benches.toml"),
        })));
        assert_eq!(finding.file, dir.join("cat.lua"));
        assert!(
            finding.problem.contains("benches.toml"),
            "{}",
            finding.problem
        );

        std::fs::write(dir.join("ok.lua"), "function bench() end\n").unwrap();
        let checked = check_script(&bench(BenchScript::File {
            path: dir.join("ok.lua"),
            declared_in: dir.join("benches.toml"),
        }));
        assert_eq!(checked, ScriptCheck::default());
    }

    #[test]
    fn every_unresolved_name_is_a_finding_at_its_first_read() {
        let checked = inline(
            "local n = 1\nfunction bench()\n  binray_with({}, \"x\")\n  helpr()\n  helpr()\nend\n",
        );
        let shown: Vec<String> = checked.findings.iter().map(|f| f.problem.clone()).collect();
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert!(
            shown[0].contains("line 3: binray_with is not defined"),
            "{shown:?}"
        );
        assert!(
            shown[1].contains("line 4: helpr is not defined"),
            "{shown:?}"
        );
    }

    #[test]
    fn builtins_library_globals_and_assigned_names_resolve() {
        let checked = inline(
            "helper = function() return 1 end\nfunction bench(params)\n  \
             local r = binary_with({}, \"x\")\n  print(tostring(error), json.encode({}), \
             string.rep(\"a\", 2), record, config, helper())\n  late()\nend\n\
             function late() end\n",
        );
        assert_eq!(checked, ScriptCheck::default());
    }

    #[test]
    fn a_script_that_never_assigns_bench_is_a_finding() {
        let finding = only(inline("local function bench() end\n"));
        assert!(
            finding.problem.contains("never assigns bench"),
            "{}",
            finding.problem
        );
    }

    #[test]
    fn reading_the_global_table_leaves_names_unchecked_and_says_so() {
        let checked = inline("_G[\"bench\"] = function() undefined() end\n");
        assert!(checked.findings.is_empty(), "{:?}", checked.findings);
        // brokkr.toml does not exist, so the lines are the string's, and the
        // warning says so as a finding would.
        let why = checked.unchecked.unwrap();
        assert!(why.contains("inline script, line 1: reads _G"), "{why}");
        assert!(why.contains("lines count from the script string"), "{why}");
    }

    #[test]
    fn a_name_is_reported_at_its_first_line_in_the_source() {
        // The walk lists the main chunk's `local unused = typo` (line 4)
        // before bench's body (line 2).
        let checked = inline("function bench()\n  typo()\nend\nlocal unused = typo\n");
        let finding = only(checked);
        assert!(
            finding.problem.contains("line 2: typo"),
            "{}",
            finding.problem
        );
    }
}
