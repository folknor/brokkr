// Sweep selection and the shared ad-hoc environment policy.

use crate::config::{CheckEntry, TestConfig};
use crate::error::DevError;
use crate::profile::{self, SweepDecl};

/// Escape sentences for a cross-entry `env` disagreement. `clippy` can pick a
/// value (`--env`) or replay one entry (`--sweep`); `check` has neither flag,
/// so its way out is to stop being ad-hoc.
const CLIPPY_ENV_CONFLICT_REMEDY: &str = "`brokkr clippy` can't pick one; pass \
     `--env KEY=...` to choose, or `--sweep NAME` to run one entry.";
const CHECK_ENV_CONFLICT_REMEDY: &str = "an ad-hoc `--features` run takes no \
     `[[check]]` entry and can't pick one; run a profile (or `-p` with no \
     `--features`) instead, or reconcile the entries' `env`.";

/// Build the list of sweep declarations both phases iterate, applying the
/// priority ladder documented at the top of the file. The caller plans them
/// once the invocation's scope is known.
///
/// Returns `Err` for an unresolved requested profile or an ad-hoc env
/// conflict across `[[check]]` entries. Every successful branch has at least
/// one sweep.
pub(crate) fn decide_active_sweeps(
    check_entries: &[CheckEntry],
    test_cfg: Option<&TestConfig>,
    profile_name: Option<&str>,
    features: &[String],
    no_default_features: bool,
) -> Result<Vec<SweepDecl>, DevError> {
    // 1. CLI override: ad-hoc one-off sweep. Overrides *sweep selection* -
    //    it takes no `[[check]]` entry, and ships no `build_packages` (the
    //    user is spot-checking; if they need a CLI rebuild they pass
    //    --package).
    //
    //    It does NOT discard the profile's run shaping. Sweep selection
    //    decides what cargo compiles; the profile's `skip`/`only`/
    //    `test_threads`/`isolation` decide which tests run and how, and the
    //    two are independent - a test that cannot pass in-process cannot
    //    pass in-process at any feature shape. This branch used to drop both
    //    together, so `check -p <pkg> --features x` issued a bare `cargo
    //    test` with no `--skip` at all and failed on tests the profile had
    //    always excluded, reporting a red indistinguishable from a code
    //    failure. `announce_adhoc_shaping` says which profile was applied.
    if !features.is_empty() || no_default_features {
        let mut sweep = SweepDecl {
            label: "default".into(),
            // The invocation named these features: never projected.
            features: profile::FeatureConfig::explicit(features, no_default_features, false),
            build_packages: Vec::new(),
            packages: Vec::new(),
            env: std::collections::BTreeMap::new(),
            ..Default::default()
        };
        if let Some(cfg) = test_cfg
            && let Some(name) = effective_profile_name(test_cfg, profile_name)?
        {
            profile::run_shaping(cfg, &name)?.apply(&mut sweep);
        }
        // Entry-level env, unioned across every `[[check]]` entry - the same
        // `merge_check_envs` `brokkr clippy`'s ad-hoc path uses, so the two
        // commands compile the same shape from the same config. Without it a
        // build-affecting invariant carried by the entries rather than the
        // profile (a var a build script reads, a codegen toggle) was silently
        // dropped, and a probe could go green having compiled something other
        // than what the gate compiles - a quiet wrong, where the dropped-skips
        // bug was a loud one. A key two entries disagree on is a hard error,
        // never a coin flip.
        //
        // This does NOT reach an invariant expressed as a cargo *feature*:
        // that follows feature resolution, which follows the CLI scope, and no
        // env union restores it.
        //
        // Entry env overlays profile env, matching `profile::build_sweep_decl`.
        //
        // `test_exclude_packages` is deliberately NOT unioned: it is a
        // per-entry selection workaround, not an invariant, so unioning it
        // would narrow what a scoped run tests while everything else about the
        // run claims to be wider. That residual stays with the caller.
        for (k, v) in merge_check_envs(
            check_entries,
            &std::collections::BTreeSet::new(),
            CHECK_ENV_CONFLICT_REMEDY,
        )? {
            sweep.env.insert(k, v);
        }
        return Ok(vec![sweep]);
    }

    // 2. Explicit --profile or default_profile from [test].
    if let Some(name) = effective_profile_name(test_cfg, profile_name)? {
        // Safe to unwrap: effective_profile_name returns Some only when
        // test_cfg is Some.
        let cfg = test_cfg.expect("test_cfg known present");
        return profile::resolve(cfg, check_entries, &name);
    }

    // 3. [[check]] entries with no profile - run every entry in order,
    //    with no libtest filters.
    if !check_entries.is_empty() {
        return Ok(check_entries
            .iter()
            .map(profile::sweep_from_check_entry)
            .collect());
    }

    // 4. Legacy fallback: `brokkr check` against a project with no
    //    `[[check]]` and no profile config. One `--all-features`
    //    invocation, matching pre-redesign behaviour. Label tracks
    //    the cargo flag so callers (e.g. `brokkr test`) don't have
    //    to special-case-detect this branch by feature-arg shape.
    Ok(vec![SweepDecl {
        label: "all-features".into(),
        features: profile::FeatureConfig {
            all_features: true,
            ..Default::default()
        },
        build_packages: Vec::new(),
        packages: Vec::new(),
        env: std::collections::BTreeMap::new(),
        ..Default::default()
    }])
}

/// Return `Some(name)` if a profile should be resolved. Errors when
/// the user passed `--profile <name>` but the project has no `[test]`
/// section at all (loud failure beats silent fallback).
pub(super) fn effective_profile_name(
    test_cfg: Option<&TestConfig>,
    profile_name: Option<&str>,
) -> Result<Option<String>, DevError> {
    match (test_cfg, profile_name) {
        (Some(_), Some(n)) => Ok(Some(n.to_owned())),
        (Some(cfg), None) => Ok(cfg.default_profile.clone()),
        (None, Some(n)) => Err(DevError::Config(format!(
            "--profile {n} requires `[test.profiles.{n}]` in brokkr.toml; \
             no `[test]` section is defined."
        ))),
        (None, None) => Ok(None),
    }
}

/// Construct the single `SweepDecl` a `brokkr clippy` invocation runs; the
/// caller plans it through the same unification resolution `check` uses.
///
/// `--sweep NAME` borrows the named `[[check]]` entry wholesale (packages,
/// features, env). Otherwise it is ad-hoc: packages from `-p`, features from the
/// flags, env unioned from every `[[check]]` entry. `--env` overrides win over
/// both and are applied last; keys an override sets are exempt from cross-sweep
/// conflict detection, so `--env` can *resolve* a conflict rather than be masked
/// by it.
pub(super) fn build_clippy_sweep(
    check_entries: &[CheckEntry],
    packages: &[String],
    all_features: bool,
    features: &[String],
    no_default_features: bool,
    sweep_name: Option<&str>,
    env_overrides: &[(String, String)],
) -> Result<SweepDecl, DevError> {
    let overridden: std::collections::BTreeSet<&str> =
        env_overrides.iter().map(|(k, _)| k.as_str()).collect();

    let mut sweep = if let Some(name) = sweep_name {
        let entry = check_entries
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| {
                DevError::Config(format!(
                    "--sweep '{name}' matches no [[check]] entry in brokkr.toml"
                ))
            })?;
        // A single entry's env is unambiguous - no cross-sweep merge needed.
        profile::sweep_from_check_entry(entry)
    } else {
        // The invocation named these features: never projected (`-p` here is
        // the sweep's own selection anyway, so there is nothing to narrow).
        let features = if all_features {
            profile::FeatureConfig::explicit(&[], false, true)
        } else {
            profile::FeatureConfig::explicit(features, no_default_features, false)
        };
        SweepDecl {
            label: "clippy".into(),
            features,
            packages: packages.to_vec(),
            env: merge_check_envs(check_entries, &overridden, CLIPPY_ENV_CONFLICT_REMEDY)?,
            ..Default::default()
        }
    };

    // `--env` overrides win last, over both the merged check env and a borrowed
    // entry's env.
    for (k, v) in env_overrides {
        sweep.env.insert(k.clone(), v.clone());
    }
    Ok(sweep)
}

/// Union the `env` of every `[[check]]` entry, erroring on a key two entries set
/// to *different* values - **unless** that key is in `overridden`, where an
/// explicit `--env` will set it anyway and the cross-sweep disagreement is moot.
///
/// Union (not intersection) is deliberate: a build-affecting invariant present
/// in most-but-not-all entries must not be silently dropped - that is the exact
/// failure BUG_SWEEP set out to prevent. The cost is that entries setting
/// *disjoint* keys contribute all of them; the escape is `--env KEY=...` (or
/// `--sweep NAME` to replay one entry exactly).
///
/// Every entry `env` is carried, load-bearing or not: telling the two apart
/// would mean knowing what each `build.rs` reads. So this guarantees the same
/// *env* on both paths, and nothing about what that env achieves - an
/// invariant a project expresses as a cargo feature follows feature
/// resolution, and no env union reaches it.
///
/// Shared by `brokkr clippy`'s ad-hoc path and `brokkr check`'s, so the two
/// commands resolve the same env from the same config - they used to disagree,
/// `clippy --features x` unioning the entries' env while `check --features x`
/// silently ran without it. `remedy` carries the escape sentence, since the
/// two commands offer different flags for it.
fn merge_check_envs(
    entries: &[CheckEntry],
    overridden: &std::collections::BTreeSet<&str>,
    remedy: &str,
) -> Result<std::collections::BTreeMap<String, String>, DevError> {
    let mut merged: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for e in entries {
        for (k, v) in &e.env {
            if overridden.contains(k.as_str()) {
                continue;
            }
            match merged.get(k) {
                Some(existing) if existing != v => {
                    return Err(DevError::Config(format!(
                        "[[check]] env conflict on `{k}`: '{existing}' vs '{v}' across \
                         sweeps - {remedy}"
                    )));
                }
                _ => {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod clippy_sweep_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::collections::BTreeSet;

    fn entry(name: &str, env: &[(&str, &str)]) -> CheckEntry {
        CheckEntry {
            name: name.into(),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..Default::default()
        }
    }

    fn overrides(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    // ----- merge_check_envs -----

    #[test]
    fn merge_unions_disjoint_keys() {
        let entries = [entry("a", &[("FOO", "1")]), entry("b", &[("BAR", "2")])];
        let merged =
            merge_check_envs(&entries, &BTreeSet::new(), CLIPPY_ENV_CONFLICT_REMEDY).unwrap();
        assert_eq!(merged.get("FOO").map(String::as_str), Some("1"));
        assert_eq!(merged.get("BAR").map(String::as_str), Some("2"));
    }

    #[test]
    fn merge_agreeing_duplicate_is_fine() {
        let entries = [entry("a", &[("HP", "1")]), entry("b", &[("HP", "1")])];
        let merged =
            merge_check_envs(&entries, &BTreeSet::new(), CLIPPY_ENV_CONFLICT_REMEDY).unwrap();
        assert_eq!(merged.get("HP").map(String::as_str), Some("1"));
    }

    #[test]
    fn merge_conflicting_duplicate_errors_naming_key() {
        let entries = [entry("a", &[("HP", "1")]), entry("b", &[("HP", "2")])];
        let err =
            merge_check_envs(&entries, &BTreeSet::new(), CLIPPY_ENV_CONFLICT_REMEDY).unwrap_err();
        assert!(err.to_string().contains("HP"), "got: {err}");
    }

    #[test]
    fn merge_conflict_is_exempt_when_overridden() {
        // The key both entries disagree on is in `overridden`, so no error - the
        // --env override will set it. (The P1-1 fix: --env resolves a conflict.)
        let entries = [entry("a", &[("HP", "1")]), entry("b", &[("HP", "2")])];
        let overridden: BTreeSet<&str> = ["HP"].into_iter().collect();
        let merged = merge_check_envs(&entries, &overridden, CLIPPY_ENV_CONFLICT_REMEDY).unwrap();
        // The overridden key is skipped entirely here; build_clippy_sweep layers
        // the override on afterwards.
        assert!(!merged.contains_key("HP"));
    }

    // ----- build_clippy_sweep: ad-hoc -----

    #[test]
    fn adhoc_all_features() {
        let s = build_clippy_sweep(&[], &[], true, &[], false, None, &[]).unwrap();
        assert_eq!(s.features.configured_key(), vec!["--all-features"]);
        // Ad-hoc features are the invocation's own: never projected.
        assert!(s.features.invocation_explicit);
        assert_eq!(s.label, "clippy");
    }

    #[test]
    fn adhoc_no_default_plus_features() {
        let feats = vec!["x".to_owned(), "y".to_owned()];
        let s = build_clippy_sweep(&[], &[], false, &feats, true, None, &[]).unwrap();
        assert_eq!(
            s.features.configured_key(),
            vec!["--no-default-features", "--features", "x,y"]
        );
        assert!(s.features.invocation_explicit);
    }

    #[test]
    fn adhoc_packages_copied() {
        let pkgs = vec!["crate_a".to_owned(), "crate_b".to_owned()];
        let s = build_clippy_sweep(&[], &pkgs, false, &[], false, None, &[]).unwrap();
        assert_eq!(s.packages, vec!["crate_a", "crate_b"]);
    }

    #[test]
    fn adhoc_env_override_resolves_conflict() {
        // Two entries disagree on HP; --env HP=chosen must make the whole thing
        // succeed AND land the chosen value on the sweep. This is the end-to-end
        // P1-1 regression: conflict no longer errors before the override applies.
        let entries = [entry("a", &[("HP", "1")]), entry("b", &[("HP", "2")])];
        let ov = overrides(&[("HP", "chosen")]);
        let s = build_clippy_sweep(&entries, &[], false, &[], false, None, &ov).unwrap();
        assert_eq!(s.env.get("HP").map(String::as_str), Some("chosen"));
    }

    #[test]
    fn adhoc_env_override_wins_over_agreeing_value() {
        let entries = [entry("a", &[("HP", "1")])];
        let ov = overrides(&[("HP", "9")]);
        let s = build_clippy_sweep(&entries, &[], false, &[], false, None, &ov).unwrap();
        assert_eq!(s.env.get("HP").map(String::as_str), Some("9"));
    }

    // ----- build_clippy_sweep: --sweep -----

    #[test]
    fn sweep_unknown_name_errors() {
        let entries = [entry("known", &[])];
        let err =
            build_clippy_sweep(&entries, &[], false, &[], false, Some("missing"), &[]).unwrap_err();
        assert!(err.to_string().contains("missing"), "got: {err}");
    }

    #[test]
    fn sweep_copies_entry_env_and_features() {
        let mut e = entry("ffi", &[("HP", "1")]);
        e.features = vec!["ffi".into()];
        e.packages = vec!["nautilus-model".into()];
        let entries = [e];
        let s = build_clippy_sweep(&entries, &[], false, &[], false, Some("ffi"), &[]).unwrap();
        assert_eq!(s.features.configured_key(), vec!["--features", "ffi"]);
        // A replayed entry's features are the entry's: projected like any
        // gate sweep's (an identity here, over the entry's own selection).
        assert!(!s.features.invocation_explicit);
        assert_eq!(s.packages, vec!["nautilus-model"]);
        assert_eq!(s.env.get("HP").map(String::as_str), Some("1"));
    }

    #[test]
    fn sweep_env_override_wins_over_entry() {
        let entries = [entry("ffi", &[("HP", "1")])];
        let ov = overrides(&[("HP", "0")]);
        let s = build_clippy_sweep(&entries, &[], false, &[], false, Some("ffi"), &ov).unwrap();
        assert_eq!(s.env.get("HP").map(String::as_str), Some("0"));
    }
}
