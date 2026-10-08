//! Which of a sweep's prebuilt harnesses hold a test matching `<NAME>`.
//!
//! A focused run used to start one `cargo test --<harness>` per harness of the
//! package and let each report "0 tests" for the name it did not contain. On a
//! package with 85 harnesses that held the lock for 17 minutes around a test
//! that passed in 0.16s - the cost was cargo, once per harness, not the test.
//! Now each prebuilt binary lists itself, directly and one at a time, and only
//! the binaries holding a match are run.
//!
//! The listing is libtest's JSON discovery (`--list --format json`), not the
//! terse form. Terse has no start or end marker, so a binary that ignored
//! `--list` and exited 0, or a listing cut short after a few valid lines, reads
//! exactly like a real, smaller listing. JSON discovery opens with a
//! `discovery` record and closes with a `completed` record carrying counts, so
//! a listing either proves itself complete or is an error - never an empty
//! answer standing in for a failed one.
//!
//! Matching is libtest's own rule, applied here to the discovered names: the
//! full name contains the filter, or equals it under `--exact`
//! (`library/test/src/lib.rs`, `filter_tests`). Benchmarks are matchable,
//! because under `cargo test` libtest runs them as tests.
//!
//! `harness = false` targets are excluded, identified from the owning package's
//! manifest - cargo's metadata and artifact records carry no harness flag. Such
//! a target's binary is arbitrary code with no libtest listing to read, and a
//! focused run selects libtest names. `harness = true` is not proof of libtest,
//! though: a `custom_test_frameworks` runner keeps it, so a target that is
//! merely *eligible* for discovery still has to produce a valid listing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::check_cmd::{DirectRuntime, TestBinary};
use crate::error::DevError;
use crate::test_runner;

/// One entry a harness reported in discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Listed {
    pub(super) name: String,
}

/// The libtest flags a discovery listing runs with.
const DISCOVERY_ARGS: [&str; 5] = ["--list", "--format", "json", "-Z", "unstable-options"];

/// What one package manifest says about harness generation, per target.
#[derive(Debug, Default)]
struct ManifestHarness {
    /// `[lib] harness = false`.
    lib_disabled: bool,
    /// `(kind, name)` of every `[[bin]]`/`[[test]]`/`[[bench]]`/`[[example]]`
    /// declaring `harness = false`.
    disabled: Vec<(String, String)>,
}

impl ManifestHarness {
    fn parse(text: &str) -> Result<Self, String> {
        let doc: toml::Table = text.parse().map_err(|e| format!("{e}"))?;
        let off = |t: &toml::Value| t.get("harness").and_then(toml::Value::as_bool) == Some(false);
        let package_name = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut out = Self { lib_disabled: doc.get("lib").is_some_and(off), disabled: Vec::new() };
        for kind in ["bin", "test", "bench", "example"] {
            let Some(entries) = doc.get(kind).and_then(toml::Value::as_array) else {
                continue;
            };
            for entry in entries.iter().filter(|e| off(e)) {
                // A `[[bin]]` may omit its name; cargo then names it for the
                // package.
                let name = entry
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .map_or_else(|| package_name.clone(), str::to_owned);
                out.disabled.push((kind.to_owned(), name));
            }
        }
        Ok(out)
    }

    fn excludes(&self, binary: &TestBinary) -> bool {
        match binary.selector_kind() {
            "lib" => self.lib_disabled,
            kind => self.disabled.iter().any(|(k, n)| k == kind && *n == binary.target),
        }
    }
}

/// The harnesses a focused run may discover, and the ones it excludes.
pub(super) struct Eligibility<'a> {
    pub(super) eligible: Vec<&'a TestBinary>,
    pub(super) excluded: Vec<&'a TestBinary>,
}

/// Split `binaries` by the `harness` flag their owning manifests declare.
pub(super) fn eligibility(binaries: &[TestBinary]) -> Result<Eligibility<'_>, DevError> {
    let mut manifests: HashMap<PathBuf, ManifestHarness> = HashMap::new();
    let mut out = Eligibility { eligible: Vec::new(), excluded: Vec::new() };
    for b in binaries {
        let path = b.manifest_dir.join("Cargo.toml");
        if !manifests.contains_key(&path) {
            let text = std::fs::read_to_string(&path).map_err(|e| {
                DevError::Config(format!(
                    "cannot read {} to tell whether `{}` is a libtest harness: {e}",
                    path.display(),
                    b.label()
                ))
            })?;
            let parsed = ManifestHarness::parse(&text).map_err(|e| {
                DevError::Config(format!("cannot parse {}: {e}", path.display()))
            })?;
            manifests.insert(path.clone(), parsed);
        }
        if manifests.get(&path).is_some_and(|m| m.excludes(b)) {
            out.excluded.push(b);
        } else {
            out.eligible.push(b);
        }
    }
    Ok(out)
}

/// List one prebuilt harness through JSON discovery, under cargo's launch
/// envelope and the per-test wall ceiling.
///
/// Every way the listing can fail to prove itself is an error naming the
/// command: a nonzero exit, a timeout, output that did not close after exit
/// (possibly truncated), or a stream without a complete discovery sequence.
pub(super) fn discover(
    binary: &TestBinary,
    runtime: &DirectRuntime,
    env: &[(&str, &str)],
    project_root: &Path,
) -> Result<Vec<Listed>, DevError> {
    let (cwd, envelope) = runtime.envelope(binary, env)?;
    let cwd = if cwd.as_os_str() == "." { project_root.to_path_buf() } else { cwd };
    let env_pairs: Vec<(&str, &str)> =
        envelope.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let run = test_runner::run_listing(
        &binary.executable,
        &DISCOVERY_ARGS,
        &cwd,
        &env_pairs,
        test_runner::TEST_TIMEOUT,
    )?;
    let command = format!("{} {}", binary.executable, DISCOVERY_ARGS.join(" "));
    let fail = |why: String| {
        let stderr = String::from_utf8_lossy(&run.stderr);
        DevError::Build(format!(
            "could not list the tests in {}: {why}\ncommand: {command}{}",
            binary.label(),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!("\n{}", stderr.trim_end())
            }
        ))
    };
    if run.timed_out {
        return Err(fail(format!(
            "the listing ran past {}s and was killed - a libtest listing answers at once, so a \
             static constructor or a custom harness is doing work (or hanging) first",
            test_runner::TEST_TIMEOUT.as_secs()
        )));
    }
    if run.unsettled {
        return Err(fail(
            "its output did not close after it exited, so the listing may be truncated".into(),
        ));
    }
    if !run.status.success() {
        return Err(fail(format!("the listing exited unsuccessfully ({})", run.status)));
    }
    let stdout = std::str::from_utf8(&run.stdout)
        .map_err(|_| fail("the listing was not valid UTF-8".into()))?;
    parse_discovery(stdout).map_err(fail)
}

/// Parse a libtest JSON discovery stream into its entries.
///
/// The stream must open with one `discovery` record, list entries, and close
/// with one `completed` record whose counts agree with what was listed.
/// Lines that are not libtest records (a static constructor printing to
/// stdout) are passed over; what makes a listing trustworthy is the bracket
/// and the counts, not the absence of noise.
fn parse_discovery(stdout: &str) -> Result<Vec<Listed>, String> {
    let mut started = false;
    let mut completed = false;
    let mut entries = Vec::new();
    let (mut tests, mut benches, mut ignored) = (0_u64, 0_u64, 0_u64);
    for line in stdout.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let kind = val.get("type").and_then(Value::as_str).unwrap_or("");
        let event = val.get("event").and_then(Value::as_str).unwrap_or("");
        match (kind, event) {
            ("suite", "discovery") => {
                if started {
                    return Err("the listing opened discovery twice".into());
                }
                started = true;
            }
            ("test" | "benchmark", "discovered") => {
                if !started || completed {
                    return Err("an entry arrived outside the discovery sequence".into());
                }
                let Some(name) = val.get("name").and_then(Value::as_str) else {
                    return Err("a discovered entry carried no name".into());
                };
                if val.get("ignore").and_then(Value::as_bool) == Some(true) {
                    ignored += 1;
                }
                if kind == "test" {
                    tests += 1;
                } else {
                    benches += 1;
                }
                entries.push(Listed { name: name.to_owned() });
            }
            ("suite", "completed") => {
                if !started || completed {
                    return Err("the listing closed discovery without opening it".into());
                }
                completed = true;
                let count = |k: &str| val.get(k).and_then(Value::as_u64);
                if count("tests") != Some(tests)
                    || count("benchmarks") != Some(benches)
                    || count("ignored") != Some(ignored)
                {
                    return Err(format!(
                        "the listing's counts ({} tests, {} benchmarks, {} ignored) disagree with \
                         the {} entries it listed",
                        count("tests").unwrap_or(0),
                        count("benchmarks").unwrap_or(0),
                        count("ignored").unwrap_or(0),
                        entries.len()
                    ));
                }
            }
            _ => {}
        }
    }
    if !started {
        return Err("it produced no libtest discovery output - not a libtest harness, or a \
                    custom one that ignores `--list --format json`"
            .into());
    }
    if !completed {
        return Err("the discovery sequence never completed - the listing was cut short".into());
    }
    Ok(entries)
}

/// libtest's filter rule: the full name contains the filter, or equals it
/// under `--exact`.
pub(super) fn matches(name: &str, filter: &str, exact: bool) -> bool {
    if exact { name == filter } else { name.contains(filter) }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn stream(lines: &[&str]) -> String {
        lines.join("\n")
    }

    const OPEN: &str = r#"{ "type": "suite", "event": "discovery" }"#;

    fn entry(kind: &str, name: &str, ignore: bool) -> String {
        format!(
            r#"{{ "type": "{kind}", "event": "discovered", "name": "{name}", "ignore": {ignore}, "ignore_message": "", "source_path": "src/lib.rs", "start_line": 1, "start_col": 1, "end_line": 1, "end_col": 2 }}"#
        )
    }

    fn close(tests: u64, benches: u64, ignored: u64) -> String {
        format!(
            r#"{{ "type": "suite", "event": "completed", "tests": {tests}, "benchmarks": {benches}, "total": {}, "ignored": {ignored} }}"#,
            tests + benches
        )
    }

    #[test]
    fn a_complete_listing_yields_tests_benchmarks_and_ignored_alike() {
        let s = stream(&[
            OPEN,
            &entry("test", "a::one", false),
            &entry("test", "a::two", true),
            &entry("benchmark", "b::bench", false),
            &close(2, 1, 1),
        ]);
        let names: Vec<String> = parse_discovery(&s).unwrap().into_iter().map(|l| l.name).collect();
        assert_eq!(names, vec!["a::one", "a::two", "b::bench"]);
    }

    // An empty harness is a real, empty listing - bracketed and counted.
    #[test]
    fn an_empty_listing_is_still_a_listing() {
        assert_eq!(parse_discovery(&stream(&[OPEN, &close(0, 0, 0)])).unwrap(), Vec::new());
    }

    // The cases terse output cannot tell apart from a smaller real listing.
    #[test]
    fn silence_truncation_and_miscounts_are_errors_not_empty_listings() {
        assert!(parse_discovery("").unwrap_err().contains("no libtest discovery output"));
        assert!(parse_discovery("my own harness ran\n").is_err());
        let truncated = stream(&[OPEN, &entry("test", "a::one", false)]);
        assert!(parse_discovery(&truncated).unwrap_err().contains("never completed"));
        let miscounted = stream(&[OPEN, &entry("test", "a::one", false), &close(2, 0, 0)]);
        assert!(parse_discovery(&miscounted).unwrap_err().contains("disagree"));
    }

    // A static constructor printing to stdout does not invalidate a listing
    // whose bracket and counts are intact.
    #[test]
    fn constructor_noise_around_a_complete_listing_is_passed_over() {
        let s = stream(&["logger initialised", OPEN, &entry("test", "a::one", false), &close(1, 0, 0)]);
        assert_eq!(parse_discovery(&s).unwrap().len(), 1);
    }

    #[test]
    fn matching_is_libtests_contains_or_equals() {
        assert!(matches("mod::emergency_reap", "reap", false));
        assert!(!matches("mod::emergency_reap", "reap", true));
        assert!(matches("mod::emergency_reap", "mod::emergency_reap", true));
    }

    fn binary(kind: &str, target: &str) -> TestBinary {
        crate::check_cmd::test_binary_for_tests("pkg", kind, target)
    }

    #[test]
    fn harness_false_targets_are_read_from_the_manifest() {
        let m = ManifestHarness::parse(
            r#"
[package]
name = "pkg"

[lib]
harness = false

[[test]]
name = "ui"
harness = false

[[test]]
name = "integration"

[[bin]]
harness = false
"#,
        )
        .unwrap();
        assert!(m.excludes(&binary("rlib", "pkg")), "[lib] harness = false");
        assert!(m.excludes(&binary("test", "ui")));
        assert!(!m.excludes(&binary("test", "integration")), "harness defaults to true");
        assert!(m.excludes(&binary("bin", "pkg")), "an unnamed [[bin]] is named for the package");
        // A same-named target of another kind is a different target.
        assert!(!m.excludes(&binary("example", "ui")));
    }
}
