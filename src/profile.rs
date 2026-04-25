//! Profile and sweep resolution for `[test.profiles.*]` / `[test.sweeps.*]`.
//!
//! Translates a named profile (with optional `extends` chain) plus the
//! ambient `TestConfig` into a list of `ResolvedSweep`s ready for the
//! test runner to execute. Each resolved sweep carries:
//!
//! - the cargo feature args for the sweep
//! - the cargo packages to rebuild before running tests (request 2)
//! - the libtest filter args derived from the merged profile fields
//!   (`tests` / `only` / `skip` / `include_ignored` / `test_threads`)
//! - any env vars the profile exports
//!
//! The resolver is intentionally pure-data: it does not run cargo, does
//! not touch disk, and does not depend on `Project`. It is reused by
//! both `brokkr check` (test phase) and `brokkr test` (single-name
//! runner).

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{FeaturesSpec, ProfileDef, SweepDef, TestConfig};
use crate::error::DevError;

/// One sweep to execute, after all profile / sweep merging is done.
#[derive(Debug, Clone)]
pub struct ResolvedSweep {
    /// Sweep label surfaced in `[test]` log lines.
    pub label: String,
    /// `["--all-features"]`, `["--features", "a,b"]`, etc. Already
    /// flattened in argv form.
    pub cargo_feature_args: Vec<String>,
    /// Packages to rebuild before running tests. Each is built with the
    /// sweep's feature args via `cargo build --release -p <pkg> ...`.
    pub build_packages: Vec<String>,
    /// `--include-ignored`, `--test-threads=N`, `--skip` flags emitted
    /// after `--` to libtest.
    pub libtest_args: Vec<String>,
    /// `--test <name>` flags emitted to cargo (before `--`).
    pub cargo_test_filters: Vec<String>,
    /// Positional substring filters passed to libtest after `--`. Each
    /// matches any test whose name contains that substring.
    pub name_filters: Vec<String>,
    /// Env vars to export to the cargo subprocess.
    pub env: BTreeMap<String, String>,
}

impl ResolvedSweep {
    /// Wall-clock-ordered argv for the libtest (post-`--`) section.
    /// Helper for tests; the runner builds argv directly.
    #[cfg(test)]
    pub fn libtest_argv(&self) -> Vec<String> {
        let mut out = self.libtest_args.clone();
        for n in &self.name_filters {
            out.push(n.clone());
        }
        out
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
/// - resolved profile names a sweep that is not in `cfg.sweeps`
/// - resolved profile has zero sweeps
pub fn resolve(cfg: &TestConfig, name: &str) -> Result<Vec<ResolvedSweep>, DevError> {
    let merged = resolve_profile_chain(&cfg.profiles, name)?;

    if merged.sweeps.is_empty() {
        return Err(DevError::Config(format!(
            "[test.profiles.{name}] resolves to zero sweeps - declare \
             `sweeps = [...]` in this profile or a parent it extends."
        )));
    }

    let mut out = Vec::with_capacity(merged.sweeps.len());
    for sweep_name in &merged.sweeps {
        let sweep_def = cfg.sweeps.get(sweep_name).ok_or_else(|| {
            DevError::Config(format!(
                "[test.profiles.{name}] references sweep '{sweep_name}', \
                 but [test.sweeps.{sweep_name}] is not defined."
            ))
        })?;
        out.push(build_resolved_sweep(sweep_name, sweep_def, &merged));
    }
    Ok(out)
}

/// Walk `name` and its `extends` ancestors, merging in child-overrides-parent
/// order. Detects missing parents and cycles up front.
fn resolve_profile_chain(
    profiles: &BTreeMap<String, ProfileDef>,
    name: &str,
) -> Result<ResolvedProfile, DevError> {
    let chain = collect_extends_chain(profiles, name)?;
    let mut out = ResolvedProfile::default();
    // chain is [child, parent, grandparent, ...]; merge from root down
    // so each step overwrites with the closer-to-leaf values.
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
            // env entries from descendants override ancestors key-by-key
            // rather than wholesale-replacing, so a child can add a single
            // var without re-stating the parent's full map.
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

fn build_resolved_sweep(
    sweep_name: &str,
    sweep: &SweepDef,
    profile: &ResolvedProfile,
) -> ResolvedSweep {
    let cargo_feature_args = sweep_feature_args(sweep);

    let mut libtest_args: Vec<String> = Vec::new();
    if profile.include_ignored {
        libtest_args.push("--include-ignored".into());
    }
    if let Some(n) = profile.test_threads {
        libtest_args.push(format!("--test-threads={n}"));
    }
    for s in &profile.skip {
        libtest_args.push("--skip".into());
        libtest_args.push(s.clone());
    }

    let mut cargo_test_filters: Vec<String> = Vec::new();
    for t in &profile.tests {
        cargo_test_filters.push("--test".into());
        cargo_test_filters.push(t.clone());
    }

    ResolvedSweep {
        label: sweep_name.to_owned(),
        cargo_feature_args,
        build_packages: sweep.build_packages.clone(),
        libtest_args,
        cargo_test_filters,
        name_filters: profile.only.clone(),
        env: profile.env.clone(),
    }
}

/// Translate a `SweepDef`'s feature/no_default_features into argv for
/// either `cargo build` or `cargo test`. Shared helper so request 2's
/// pre-build step and the test runner agree on the feature flags.
pub fn sweep_feature_args(sweep: &SweepDef) -> Vec<String> {
    let mut args = Vec::new();
    if sweep.no_default_features {
        args.push("--no-default-features".into());
    }
    match &sweep.features {
        Some(FeaturesSpec::All) => args.push("--all-features".into()),
        Some(FeaturesSpec::List(list)) if !list.is_empty() => {
            args.push("--features".into());
            args.push(list.join(","));
        }
        // Some(List) empty or None: emit nothing (cargo defaults apply).
        _ => {}
    }
    args
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

    fn cfg_from_toml(text: &str) -> TestConfig {
        let parsed: BTreeMap<String, toml::Value> = toml::from_str(text).unwrap();
        let value = parsed
            .get("test")
            .cloned()
            .unwrap_or(toml::Value::Table(Default::default()));
        value.try_into().unwrap()
    }

    #[test]
    fn sweep_feature_args_all() {
        let s = SweepDef {
            features: Some(FeaturesSpec::All),
            no_default_features: false,
            build_packages: Vec::new(),
        };
        assert_eq!(sweep_feature_args(&s), vec!["--all-features"]);
    }

    #[test]
    fn sweep_feature_args_consumer_shape() {
        let s = SweepDef {
            features: Some(FeaturesSpec::List(vec!["commands".into()])),
            no_default_features: true,
            build_packages: vec!["pbfhogg-cli".into()],
        };
        assert_eq!(
            sweep_feature_args(&s),
            vec!["--no-default-features", "--features", "commands"]
        );
    }

    #[test]
    fn sweep_feature_args_no_default_no_features() {
        let s = SweepDef {
            features: None,
            no_default_features: true,
            build_packages: Vec::new(),
        };
        assert_eq!(sweep_feature_args(&s), vec!["--no-default-features"]);
    }

    #[test]
    fn sweep_feature_args_empty_list_emits_nothing() {
        let s = SweepDef {
            features: Some(FeaturesSpec::List(Vec::new())),
            no_default_features: false,
            build_packages: Vec::new(),
        };
        assert!(sweep_feature_args(&s).is_empty());
    }

    #[test]
    fn resolve_simple_profile() {
        let cfg = cfg_from_toml(
            r#"
[test.sweeps.all]
features = "all"
build_packages = ["pbfhogg-cli"]

[test.profiles.tier1]
sweeps = ["all"]
skip = ["tier2::", "platform::"]
include_ignored = false
"#,
        );
        let resolved = resolve(&cfg, "tier1").unwrap();
        assert_eq!(resolved.len(), 1);
        let s = &resolved[0];
        assert_eq!(s.label, "all");
        assert_eq!(s.cargo_feature_args, vec!["--all-features"]);
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
        let cfg = cfg_from_toml(
            r#"
[test.sweeps.all]
features = "all"

[test.sweeps.consumer]
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
        let resolved = resolve(&cfg, "sort").unwrap();
        assert_eq!(resolved.len(), 2);

        let s0 = &resolved[0];
        assert_eq!(s0.label, "all");
        assert_eq!(s0.cargo_feature_args, vec!["--all-features"]);
        // tier2:: is gone; only platform:: + serial:: remain in skip.
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
        let cfg = cfg_from_toml(
            r#"
[test.sweeps.all]
features = "all"

[test.profiles.serial]
sweeps = ["all"]
only = ["serial::"]
include_ignored = true
test_threads = 1
env = { BROKKR_TEST_PLATFORM = "1" }
"#,
        );
        let r = resolve(&cfg, "serial").unwrap();
        assert_eq!(r[0].name_filters, vec!["serial::"]);
        assert!(
            r[0].libtest_args.contains(&"--include-ignored".into()),
            "got: {:?}",
            r[0].libtest_args
        );
        assert!(
            r[0].libtest_args.contains(&"--test-threads=1".into()),
            "got: {:?}",
            r[0].libtest_args
        );
        assert_eq!(r[0].env.get("BROKKR_TEST_PLATFORM").map(String::as_str), Some("1"));
    }

    #[test]
    fn resolve_unknown_profile_errors() {
        let cfg = cfg_from_toml(
            r#"
[test.sweeps.all]
features = "all"
"#,
        );
        let err = resolve(&cfg, "nope").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nope"), "got: {msg}");
    }

    #[test]
    fn resolve_unknown_sweep_errors() {
        let cfg = cfg_from_toml(
            r#"
[test.profiles.tier1]
sweeps = ["nope"]
"#,
        );
        let err = resolve(&cfg, "tier1").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'nope'"), "got: {msg}");
    }

    #[test]
    fn resolve_extends_cycle_errors() {
        let cfg = cfg_from_toml(
            r#"
[test.sweeps.all]
features = "all"

[test.profiles.a]
extends = "b"
sweeps = ["all"]

[test.profiles.b]
extends = "a"
sweeps = ["all"]
"#,
        );
        let err = resolve(&cfg, "a").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cycle"), "got: {msg}");
    }

    #[test]
    fn resolve_zero_sweeps_errors() {
        let cfg = cfg_from_toml(
            r#"
[test.sweeps.all]
features = "all"

[test.profiles.empty]
description = "forgot to set sweeps"
"#,
        );
        let err = resolve(&cfg, "empty").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("zero sweeps"), "got: {msg}");
    }

    #[test]
    fn resolve_extends_chain_three_levels() {
        // grandparent -> parent -> child; each layer adds one piece.
        let cfg = cfg_from_toml(
            r#"
[test.sweeps.all]
features = "all"

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
        let r = resolve(&cfg, "ch").unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].cargo_test_filters, vec!["--test", "cli_x"]);
        assert!(r[0].libtest_args.contains(&"--include-ignored".into()));
        assert_eq!(r[0].libtest_args.iter().filter(|s| s.as_str() == "a::").count(), 1);
    }

    #[test]
    fn libtest_argv_concats_args_and_name_filters() {
        let s = ResolvedSweep {
            label: "x".into(),
            cargo_feature_args: Vec::new(),
            build_packages: Vec::new(),
            libtest_args: vec!["--include-ignored".into()],
            cargo_test_filters: Vec::new(),
            name_filters: vec!["tier2::".into()],
            env: BTreeMap::new(),
        };
        assert_eq!(s.libtest_argv(), vec!["--include-ignored", "tier2::"]);
    }
}
