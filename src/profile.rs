//! Profile resolution for `[test.profiles.*]`.
//!
//! Translates a named profile (with optional `extends` chain) into a
//! list of `ResolvedSweep`s ready for `brokkr check` (and
//! `brokkr test`) to execute. Each resolved sweep carries:
//!
//! - the `[[check]]` entry that supplies feature flags + build_packages
//! - the libtest filter args derived from the merged profile fields
//!   (`tests` / `only` / `skip` / `include_ignored` / `test_threads`)
//! - any env vars the profile exports
//!
//! The resolver is intentionally pure-data: it does not run cargo, does
//! not touch disk, and does not depend on `Project`.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{CheckEntry, ProfileDef, TestConfig};
use crate::error::DevError;

/// One sweep to execute, after profile resolution + check-entry lookup.
#[derive(Debug, Clone, Default)]
pub struct ResolvedSweep {
    /// Display label - the `[[check]]` entry's `name`.
    pub label: String,
    /// `["--all-features"]`, `["--features", "a,b"]`, etc. Already
    /// flattened in argv form, derived from the entry's flags.
    pub cargo_feature_args: Vec<String>,
    /// Packages to rebuild before running tests, sourced from the
    /// resolved `[[check]]` entry. `cargo build --release -p <pkg>`
    /// with the same feature args.
    pub build_packages: Vec<String>,
    /// Packages to scope the clippy / test invocation to, emitted as
    /// `-p <pkg>`. From the `[[check]]` entry's `packages`. Needed for
    /// `--features` to be valid in a virtual workspace.
    pub packages: Vec<String>,
    /// Packages to omit from the test invocation, emitted as
    /// `--workspace --exclude <pkg>`. From the `[[check]]` entry's
    /// `test_exclude_packages`. Test phase only; clippy stays workspace-wide.
    pub test_exclude_packages: Vec<String>,
    /// The profile's resolved `test_threads`, carried raw so the check test
    /// phase can decide serial vs parallel: `None`/`Some(1)` run under the
    /// per-test watchdog (`--test-threads=1`); `Some(0)` runs at libtest's
    /// default parallelism; `Some(n>=2)` runs `--test-threads=n`. The last two
    /// bypass the watchdog for a whole-sweep timeout. `brokkr test` ignores
    /// this and is always serial.
    pub test_threads: Option<u32>,
    /// `--include-ignored`, `--test-threads=N`, `--skip` flags emitted
    /// after `--` to libtest. Derived from the merged profile.
    pub libtest_args: Vec<String>,
    /// `--test <name>` flags emitted to cargo (before `--`).
    pub cargo_test_filters: Vec<String>,
    /// Positional substring filters passed to libtest after `--`.
    pub name_filters: Vec<String>,
    /// Env vars to export to the cargo subprocess.
    pub env: BTreeMap<String, String>,
    /// Extra `rustc` flags appended to `RUSTFLAGS` for this sweep's cargo runs.
    /// A non-empty value also auto-isolates the sweep's target dir; the
    /// execution layer (`sweep_runtime_env`) turns these into the `RUSTFLAGS`
    /// and `CARGO_TARGET_DIR` env pair. From the `[[check]]` entry's `rustflags`.
    pub rustflags: Vec<String>,
}

impl ResolvedSweep {
    /// Wall-clock-ordered argv for the libtest (post-`--`) section.
    /// Helper for tests.
    #[cfg(test)]
    pub fn libtest_argv(&self) -> Vec<String> {
        let mut out = self.libtest_args.clone();
        for n in &self.name_filters {
            out.push(n.clone());
        }
        out
    }
}

/// Synthesize a `ResolvedSweep` from a `CheckEntry` alone, with no
/// profile filters. Used by the bare `brokkr check` path when
/// `[[check]]` is configured but no profile is selected.
pub fn sweep_from_check_entry(entry: &CheckEntry) -> ResolvedSweep {
    // No profile in play, so the entry's own filters are all there is.
    let mut libtest_args: Vec<String> = Vec::new();
    for s in &entry.skip {
        libtest_args.push("--skip".into());
        libtest_args.push(s.clone());
    }
    let mut cargo_test_filters: Vec<String> = Vec::new();
    for t in &entry.tests {
        cargo_test_filters.push("--test".into());
        cargo_test_filters.push(t.clone());
    }

    ResolvedSweep {
        label: entry.name.clone(),
        cargo_feature_args: entry.cargo_feature_args(),
        build_packages: entry.build_packages.clone(),
        packages: entry.packages.clone(),
        test_exclude_packages: entry.test_exclude_packages.clone(),
        libtest_args,
        cargo_test_filters,
        name_filters: entry.only.clone(),
        env: entry.env.clone(),
        test_threads: None,
        rustflags: entry.rustflags.clone(),
    }
}

/// Merged, resolved view of a `ProfileDef` after walking its `extends`
/// chain. Collection fields default to empty when nothing was set
/// anywhere in the chain.
#[derive(Debug, Clone, Default)]
struct ResolvedProfile {
    sweeps: Vec<String>,
    tests: Vec<String>,
    only: Vec<String>,
    skip: Vec<String>,
    include_ignored: bool,
    test_threads: Option<u32>,
    env: BTreeMap<String, String>,
}

/// Resolve `name` into a list of `ResolvedSweep`s ready to execute.
///
/// Errors:
/// - `name` is not in `cfg.profiles`
/// - `extends` chain refers to a missing profile
/// - `extends` chain contains a cycle
/// - resolved profile names a sweep that is not in `checks`
/// - resolved profile has zero sweeps
pub fn resolve(
    cfg: &TestConfig,
    checks: &[CheckEntry],
    name: &str,
) -> Result<Vec<ResolvedSweep>, DevError> {
    let merged = resolve_profile_chain(&cfg.profiles, name)?;

    if merged.sweeps.is_empty() {
        return Err(DevError::Config(format!(
            "[test.profiles.{name}] resolves to zero sweeps - declare \
             `sweeps = [...]` in this profile or a parent it extends."
        )));
    }

    let mut out = Vec::with_capacity(merged.sweeps.len());
    for sweep_name in &merged.sweeps {
        let entry = checks
            .iter()
            .find(|e| e.name == *sweep_name)
            .ok_or_else(|| {
                DevError::Config(format!(
                    "[test.profiles.{name}] references sweep '{sweep_name}', \
                     but no `[[check]]` entry with that name exists."
                ))
            })?;
        out.push(build_resolved_sweep(entry, &merged));
    }
    Ok(out)
}

/// Walk `name` and its `extends` ancestors, merging in
/// child-overrides-parent order. Detects missing parents and cycles
/// up front.
fn resolve_profile_chain(
    profiles: &BTreeMap<String, ProfileDef>,
    name: &str,
) -> Result<ResolvedProfile, DevError> {
    let chain = collect_extends_chain(profiles, name)?;
    let mut out = ResolvedProfile::default();
    for def in chain.iter().rev() {
        if let Some(v) = &def.sweeps {
            out.sweeps = v.clone();
        }
        if let Some(v) = &def.tests {
            out.tests = v.clone();
        }
        if let Some(v) = &def.only {
            out.only = v.clone();
        }
        if let Some(v) = &def.skip {
            out.skip = v.clone();
        }
        if let Some(v) = def.include_ignored {
            out.include_ignored = v;
        }
        if let Some(v) = def.test_threads {
            out.test_threads = Some(v);
        }
        if let Some(v) = &def.env {
            for (k, val) in v {
                out.env.insert(k.clone(), val.clone());
            }
        }
    }
    Ok(out)
}

fn collect_extends_chain<'a>(
    profiles: &'a BTreeMap<String, ProfileDef>,
    name: &str,
) -> Result<Vec<&'a ProfileDef>, DevError> {
    let mut chain: Vec<&ProfileDef> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut cur = name.to_owned();
    loop {
        if !seen.insert(cur.clone()) {
            return Err(DevError::Config(format!(
                "[test.profiles] extends-cycle detected at '{cur}' \
                 (visited: {})",
                seen.iter().cloned().collect::<Vec<_>>().join(" -> ")
            )));
        }
        let def = profiles.get(&cur).ok_or_else(|| {
            DevError::Config(format!("[test.profiles.{cur}] is not defined"))
        })?;
        chain.push(def);
        match &def.extends {
            Some(parent) => cur = parent.clone(),
            None => return Ok(chain),
        }
    }
}

fn build_resolved_sweep(entry: &CheckEntry, profile: &ResolvedProfile) -> ResolvedSweep {
    // `test_threads` is NOT pushed into `libtest_args` here. It is carried raw
    // on the sweep so each consumer applies its own policy: the check test
    // phase turns it into `--test-threads=N` (or a parallel run), while
    // `brokkr test` ignores it and forces `--test-threads=1`.
    let mut libtest_args: Vec<String> = Vec::new();
    if profile.include_ignored {
        libtest_args.push("--include-ignored".into());
    }
    // Profile `skip` then the entry's own `skip` - they AND (both apply), never
    // replace, so a sweep can pin its own exclusions on top of the profile's.
    for s in profile.skip.iter().chain(&entry.skip) {
        libtest_args.push("--skip".into());
        libtest_args.push(s.clone());
    }

    let mut cargo_test_filters: Vec<String> = Vec::new();
    for t in profile.tests.iter().chain(&entry.tests) {
        cargo_test_filters.push("--test".into());
        cargo_test_filters.push(t.clone());
    }

    // Profile `only` (positional substring filters) then the entry's own.
    let mut name_filters = profile.only.clone();
    name_filters.extend(entry.only.iter().cloned());

    // Profile env is the base; the entry's own env overlays it so a
    // sweep-specific var wins on a key collision.
    let mut env = profile.env.clone();
    for (k, v) in &entry.env {
        env.insert(k.clone(), v.clone());
    }

    ResolvedSweep {
        label: entry.name.clone(),
        cargo_feature_args: entry.cargo_feature_args(),
        build_packages: entry.build_packages.clone(),
        packages: entry.packages.clone(),
        test_exclude_packages: entry.test_exclude_packages.clone(),
        libtest_args,
        cargo_test_filters,
        name_filters,
        env,
        test_threads: profile.test_threads,
        rustflags: entry.rustflags.clone(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines
    )]
    use super::*;

    /// Parse the body of a fake `brokkr.toml`-shaped fragment into
    /// `(checks, test_cfg)`. Keeps the test cases readable and avoids
    /// hand-building TestConfig + Vec<CheckEntry> in every assertion.
    fn parse_fragment(text: &str) -> (Vec<CheckEntry>, TestConfig) {
        let v: toml::Value = toml::from_str(text).unwrap();
        let table = v.as_table().unwrap();
        let checks: Vec<CheckEntry> = table
            .get("check")
            .map(|c| c.clone().try_into().unwrap())
            .unwrap_or_default();
        let test_cfg: TestConfig = table
            .get("test")
            .map(|t| t.clone().try_into().unwrap())
            .unwrap_or_default();
        (checks, test_cfg)
    }

    #[test]
    fn sweep_from_check_entry_emits_feature_args() {
        let entry = CheckEntry {
            name: "consumer".into(),
            features: vec!["commands".into()],
            no_default_features: true,
            build_packages: vec!["pbfhogg-cli".into()],
            ..Default::default()
        };
        let s = sweep_from_check_entry(&entry);
        assert_eq!(s.label, "consumer");
        assert_eq!(
            s.cargo_feature_args,
            vec!["--no-default-features", "--features", "commands"]
        );
        assert_eq!(s.build_packages, vec!["pbfhogg-cli"]);
        assert!(s.libtest_args.is_empty());
        assert!(s.cargo_test_filters.is_empty());
        assert!(s.name_filters.is_empty());
    }

    #[test]
    fn sweep_from_check_entry_carries_rustflags_and_own_filters() {
        let entry = CheckEntry {
            name: "sim".into(),
            rustflags: vec!["--cfg".into(), "madsim".into()],
            tests: vec!["reconciliation".into()],
            skip: vec!["flaky::".into()],
            only: vec!["virtual_time".into()],
            ..Default::default()
        };
        let s = sweep_from_check_entry(&entry);
        assert_eq!(s.rustflags, vec!["--cfg", "madsim"]);
        assert_eq!(s.cargo_test_filters, vec!["--test", "reconciliation"]);
        assert_eq!(s.libtest_args, vec!["--skip", "flaky::"]);
        assert_eq!(s.name_filters, vec!["virtual_time"]);
    }

    #[test]
    fn per_check_filters_append_after_profile_filters() {
        // Profile filters come first, the entry's own filters append (AND
        // semantics), and the entry's rustflags propagate onto the sweep.
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "sim"
rustflags = ["--cfg", "madsim"]
skip = ["entry_skip::"]
tests = ["entry_test"]
only = ["entry_only"]

[test.profiles.sim]
sweeps = ["sim"]
skip = ["profile_skip::"]
tests = ["profile_test"]
only = ["profile_only"]
"#,
        );
        let resolved = resolve(&cfg, &checks, "sim").unwrap();
        let s = &resolved[0];
        assert_eq!(s.rustflags, vec!["--cfg", "madsim"]);
        assert_eq!(
            s.libtest_args,
            vec!["--skip", "profile_skip::", "--skip", "entry_skip::"]
        );
        assert_eq!(
            s.cargo_test_filters,
            vec!["--test", "profile_test", "--test", "entry_test"]
        );
        assert_eq!(s.name_filters, vec!["profile_only", "entry_only"]);
    }

    #[test]
    fn resolve_simple_profile() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "all"
features = ["a", "b"]
build_packages = ["pbfhogg-cli"]

[test.profiles.tier1]
sweeps = ["all"]
skip = ["tier2::", "platform::"]
include_ignored = false
"#,
        );
        let resolved = resolve(&cfg, &checks, "tier1").unwrap();
        assert_eq!(resolved.len(), 1);
        let s = &resolved[0];
        assert_eq!(s.label, "all");
        assert_eq!(s.cargo_feature_args, vec!["--features", "a,b"]);
        assert_eq!(s.build_packages, vec!["pbfhogg-cli"]);
        assert_eq!(
            s.libtest_args,
            vec!["--skip", "tier2::", "--skip", "platform::"]
        );
        assert!(s.cargo_test_filters.is_empty());
        assert!(s.name_filters.is_empty());
    }

    #[test]
    fn resolve_extends_replaces_collections() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "all"
features = ["a"]

[[check]]
name = "consumer"
no_default_features = true
features = ["commands"]

[test.profiles.tier1]
sweeps = ["all", "consumer"]
skip = ["tier2::", "tier3::", "platform::", "serial::"]
include_ignored = false

# Sort extends tier1 but ships its own skip list, intentionally letting
# tier2:: through. Collections replace, not append.
[test.profiles.sort]
extends = "tier1"
tests = ["cli_sort"]
skip = ["platform::", "serial::"]
"#,
        );
        let resolved = resolve(&cfg, &checks, "sort").unwrap();
        assert_eq!(resolved.len(), 2);

        let s0 = &resolved[0];
        assert_eq!(s0.label, "all");
        assert_eq!(s0.cargo_feature_args, vec!["--features", "a"]);
        assert_eq!(
            s0.libtest_args,
            vec!["--skip", "platform::", "--skip", "serial::"]
        );
        assert_eq!(s0.cargo_test_filters, vec!["--test", "cli_sort"]);

        let s1 = &resolved[1];
        assert_eq!(s1.label, "consumer");
        assert_eq!(
            s1.cargo_feature_args,
            vec!["--no-default-features", "--features", "commands"]
        );
    }

    #[test]
    fn resolve_propagates_test_threads_and_env() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "all"
features = ["a"]

[test.profiles.serial]
sweeps = ["all"]
only = ["serial::"]
include_ignored = true
test_threads = 1
env = { BROKKR_TEST_PLATFORM = "1" }
"#,
        );
        let r = resolve(&cfg, &checks, "serial").unwrap();
        assert_eq!(r[0].name_filters, vec!["serial::"]);
        assert!(
            r[0].libtest_args.contains(&"--include-ignored".into()),
            "got: {:?}",
            r[0].libtest_args
        );
        // test_threads is carried raw on the sweep, not pushed into
        // libtest_args (consumers apply their own thread policy).
        assert_eq!(r[0].test_threads, Some(1));
        assert!(
            !r[0].libtest_args.iter().any(|a| a.starts_with("--test-threads")),
            "test_threads must not be pushed into libtest_args: {:?}",
            r[0].libtest_args
        );
        assert_eq!(r[0].env.get("BROKKR_TEST_PLATFORM").map(String::as_str), Some("1"));
    }

    #[test]
    fn resolve_carries_packages_and_merges_entry_env_over_profile() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "core"
features = ["high-precision"]
packages = ["nautilus-core", "nautilus-common"]
env = { HIGH_PRECISION = "1", SHARED = "entry" }

[test.profiles.p]
sweeps = ["core"]
env = { SHARED = "profile", ONLY_PROFILE = "1" }
"#,
        );
        let r = resolve(&cfg, &checks, "p").unwrap();
        assert_eq!(r[0].packages, vec!["nautilus-core", "nautilus-common"]);
        // Entry env present, profile-only env present, and the entry wins the
        // `SHARED` collision.
        assert_eq!(r[0].env.get("HIGH_PRECISION").map(String::as_str), Some("1"));
        assert_eq!(r[0].env.get("ONLY_PROFILE").map(String::as_str), Some("1"));
        assert_eq!(r[0].env.get("SHARED").map(String::as_str), Some("entry"));
    }

    #[test]
    fn sweep_from_check_entry_carries_packages_and_env() {
        let (checks, _cfg) = parse_fragment(
            r#"
[[check]]
name = "core"
packages = ["nautilus-core"]
env = { HIGH_PRECISION = "1" }
"#,
        );
        let s = sweep_from_check_entry(&checks[0]);
        assert_eq!(s.packages, vec!["nautilus-core"]);
        assert_eq!(s.env.get("HIGH_PRECISION").map(String::as_str), Some("1"));
    }

    #[test]
    fn sweep_carries_test_exclude_packages() {
        let (checks, _cfg) = parse_fragment(
            r#"
[[check]]
name = "ws"
test_exclude_packages = ["nautilus-pyo3", "nautilus-python"]
"#,
        );
        let s = sweep_from_check_entry(&checks[0]);
        assert_eq!(
            s.test_exclude_packages,
            vec!["nautilus-pyo3", "nautilus-python"]
        );
        // Clippy scoping (`packages`) stays empty - excludes are test-only.
        assert!(s.packages.is_empty());
    }

    #[test]
    fn resolve_carries_parallel_test_threads() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "all"
features = ["a"]

[test.profiles.fast]
sweeps = ["all"]
test_threads = 8
"#,
        );
        let r = resolve(&cfg, &checks, "fast").unwrap();
        assert_eq!(r[0].test_threads, Some(8));
        assert!(!r[0].libtest_args.iter().any(|a| a.starts_with("--test-threads")));
    }

    #[test]
    fn resolve_unknown_profile_errors() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "all"
features = ["a"]
"#,
        );
        let err = resolve(&cfg, &checks, "nope").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nope"), "got: {msg}");
    }

    #[test]
    fn resolve_unknown_sweep_errors_at_resolve_time() {
        // (Top-level config also catches this at parse time via
        // `validate_check_against_test`, but the resolver is the
        // last line of defence and shouldn't panic.)
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "all"
features = ["a"]

[test.profiles.tier1]
sweeps = ["nope"]
"#,
        );
        let err = resolve(&cfg, &checks, "tier1").unwrap_err();
        assert!(err.to_string().contains("'nope'"), "got: {err}");
    }

    #[test]
    fn resolve_extends_cycle_errors() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "all"
features = ["a"]

[test.profiles.a]
extends = "b"
sweeps = ["all"]

[test.profiles.b]
extends = "a"
sweeps = ["all"]
"#,
        );
        let err = resolve(&cfg, &checks, "a").unwrap_err();
        assert!(err.to_string().contains("cycle"), "got: {err}");
    }

    #[test]
    fn resolve_zero_sweeps_errors() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "all"
features = ["a"]

[test.profiles.empty]
description = "forgot to set sweeps"
"#,
        );
        let err = resolve(&cfg, &checks, "empty").unwrap_err();
        assert!(err.to_string().contains("zero sweeps"), "got: {err}");
    }

    #[test]
    fn resolve_extends_chain_three_levels() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "all"
features = ["a"]

[test.profiles.gp]
sweeps = ["all"]
skip = ["a::"]

[test.profiles.par]
extends = "gp"
include_ignored = true

[test.profiles.ch]
extends = "par"
tests = ["cli_x"]
"#,
        );
        let r = resolve(&cfg, &checks, "ch").unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].cargo_test_filters, vec!["--test", "cli_x"]);
        assert!(r[0].libtest_args.contains(&"--include-ignored".into()));
        assert_eq!(
            r[0].libtest_args.iter().filter(|s| s.as_str() == "a::").count(),
            1
        );
    }

    #[test]
    fn libtest_argv_concats_args_and_name_filters() {
        let s = ResolvedSweep {
            label: "x".into(),
            cargo_feature_args: Vec::new(),
            build_packages: Vec::new(),
            packages: Vec::new(),
            libtest_args: vec!["--include-ignored".into()],
            cargo_test_filters: Vec::new(),
            name_filters: vec!["tier2::".into()],
            env: BTreeMap::new(),
            ..Default::default()
        };
        assert_eq!(s.libtest_argv(), vec!["--include-ignored", "tier2::"]);
    }
}
