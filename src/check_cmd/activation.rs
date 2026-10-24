// Sweep selection and the shared ad-hoc environment policy.

use crate::config::{CheckEntry, TestConfig};
use crate::error::DevError;
use crate::profile::{self, SweepDecl};

#[derive(Clone, Copy)]
enum ActivationPurpose {
    Check,
    Clippy,
}

struct SweepPolicy {
    always_adhoc: bool,
    profile_shaping: bool,
    label: &'static str,
    env_remedy: &'static str,
}

impl ActivationPurpose {
    fn sweep_policy(self) -> SweepPolicy {
        match self {
            Self::Clippy => SweepPolicy {
                always_adhoc: true,
                profile_shaping: false,
                label: "clippy",
                env_remedy: "`brokkr clippy` can't pick one; pass `--env KEY=...` to choose, or `--sweep NAME` to run one entry.",
            },
            Self::Check => SweepPolicy {
                always_adhoc: false,
                profile_shaping: true,
                label: "default",
                env_remedy: "an ad-hoc `--features` run takes no `[[check]]` entry and can't pick one; run a profile (or `-p` with no `--features`) instead, or reconcile the entries' `env`.",
            },
        }
    }
}

/// Build the list of sweep declarations both phases iterate, applying the
/// priority ladder from the check-purpose policy. The caller plans them
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
    resolve_sweeps(
        ActivationPurpose::Check,
        SweepInputs {
            check_entries,
            test_cfg,
            profile_name,
            features,
            no_default_features,
            all_features: false,
            sweep_name: None,
            packages: &[],
            env_overrides: &[],
        },
    )
}

#[derive(Clone, Copy)]
struct SweepInputs<'a> {
    check_entries: &'a [CheckEntry],
    test_cfg: Option<&'a TestConfig>,
    profile_name: Option<&'a str>,
    features: &'a [String],
    no_default_features: bool,
    all_features: bool,
    sweep_name: Option<&'a str>,
    packages: &'a [String],
    env_overrides: &'a [(String, String)],
}

fn resolve_sweeps(
    purpose: ActivationPurpose,
    inputs: SweepInputs<'_>,
) -> Result<Vec<SweepDecl>, DevError> {
    let policy = purpose.sweep_policy();
    let overridden = inputs
        .env_overrides
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    let mut sweeps = if let Some(name) = inputs.sweep_name {
        let entry = inputs
            .check_entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| {
                DevError::Config(format!(
                    "--sweep '{name}' matches no [[check]] entry in brokkr.toml"
                ))
            })?;
        vec![profile::sweep_from_check_entry(entry)]
    } else if policy.always_adhoc || !inputs.features.is_empty() || inputs.no_default_features {
        let mut sweep = SweepDecl {
            label: policy.label.into(),
            features: profile::FeatureConfig::explicit(
                if inputs.all_features {
                    &[]
                } else {
                    inputs.features
                },
                !inputs.all_features && inputs.no_default_features,
                inputs.all_features,
            ),
            packages: inputs.packages.to_vec(),
            ..Default::default()
        };
        // Feature overrides replace build selection, not a named profile's
        // run shaping. Selection decides what cargo compiles; the profile's
        // `skip`/`only`/`test_threads`/`isolation` decide which tests run and
        // how, and a test that cannot pass in-process cannot pass at any
        // feature shape. Dropping both together once made `check -p <pkg>
        // --features x` fail on tests the profile always excluded, a red
        // indistinguishable from a code failure. Profile env is part of the
        // shaping; entry env overlays it below.
        //
        // The entry env union is what keeps an ad-hoc probe compiling the
        // gate's shape: an invariant a build script reads from the env would
        // otherwise be dropped silently. It does not reach an invariant
        // expressed as a cargo feature, and `test_exclude_packages` is
        // deliberately not unioned (a per-entry selection workaround, not an
        // invariant).
        if policy.profile_shaping
            && let Some(cfg) = inputs.test_cfg
            && let Some(name) = effective_profile_name(inputs.test_cfg, inputs.profile_name)?
        {
            profile::run_shaping(cfg, &name)?.apply(&mut sweep);
        }
        sweep.env.extend(merge_check_envs(
            inputs.check_entries,
            &overridden,
            policy.env_remedy,
        )?);
        vec![sweep]
    } else if let Some(name) = effective_profile_name(inputs.test_cfg, inputs.profile_name)? {
        let cfg = inputs
            .test_cfg
            .expect("resolved profile requires test config");
        profile::resolve(cfg, inputs.check_entries, &name)?
    } else if !inputs.check_entries.is_empty() {
        inputs
            .check_entries
            .iter()
            .map(profile::sweep_from_check_entry)
            .collect()
    } else {
        vec![SweepDecl {
            label: "all-features".into(),
            features: profile::FeatureConfig {
                all_features: true,
                ..Default::default()
            },
            ..Default::default()
        }]
    };
    for sweep in &mut sweeps {
        sweep.env.extend(inputs.env_overrides.iter().cloned());
    }
    // Every path a test run's sweeps take passes here - an entry on its own,
    // a profile, an ad-hoc sweep carrying a profile's shaping - so an
    // exclusion no lane can honour is refused once, whichever built it.
    // Clippy runs no tests: there is nothing for an exclusion to leave out.
    if matches!(purpose, ActivationPurpose::Check) {
        for sweep in &sweeps {
            profile::refuse_unhonoured_exclusions(sweep)?;
        }
    }
    Ok(sweeps)
}

/// Return `Some(name)` if a profile should be resolved. Errors when
/// the user passed `--profile <name>` but the project has no `[test]`
/// section at all (loud failure beats silent fallback).
pub(crate) fn effective_profile_name(
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
pub(crate) fn build_clippy_sweep(
    check_entries: &[CheckEntry],
    packages: &[String],
    all_features: bool,
    features: &[String],
    no_default_features: bool,
    sweep_name: Option<&str>,
    env_overrides: &[(String, String)],
) -> Result<SweepDecl, DevError> {
    let mut sweeps = resolve_sweeps(
        ActivationPurpose::Clippy,
        SweepInputs {
            check_entries,
            test_cfg: None,
            profile_name: None,
            features,
            no_default_features,
            all_features,
            sweep_name,
            packages,
            env_overrides,
        },
    )?;
    // The investigative purpose always produces exactly one declaration.
    Ok(sweeps.remove(0))
}

/// Union the `env` of every `[[check]]` entry, erroring on a key two entries set
/// to *different* values - **unless** that key is in `overridden`, where an
/// explicit `--env` will set it anyway and the cross-sweep disagreement is moot.
///
/// Union (not intersection) is deliberate: a build-affecting invariant present
/// in most-but-not-all entries must not be silently dropped - that is the exact
/// failure a dropped build-affecting variable can cause. Entries setting
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
        let merged = merge_check_envs(
            &entries,
            &BTreeSet::new(),
            ActivationPurpose::Clippy.sweep_policy().env_remedy,
        )
        .unwrap();
        assert_eq!(merged.get("FOO").map(String::as_str), Some("1"));
        assert_eq!(merged.get("BAR").map(String::as_str), Some("2"));
    }

    #[test]
    fn merge_agreeing_duplicate_is_fine() {
        let entries = [entry("a", &[("HP", "1")]), entry("b", &[("HP", "1")])];
        let merged = merge_check_envs(
            &entries,
            &BTreeSet::new(),
            ActivationPurpose::Clippy.sweep_policy().env_remedy,
        )
        .unwrap();
        assert_eq!(merged.get("HP").map(String::as_str), Some("1"));
    }

    #[test]
    fn merge_conflicting_duplicate_errors_naming_key() {
        let entries = [entry("a", &[("HP", "1")]), entry("b", &[("HP", "2")])];
        let err = merge_check_envs(
            &entries,
            &BTreeSet::new(),
            ActivationPurpose::Clippy.sweep_policy().env_remedy,
        )
        .unwrap_err();
        assert!(err.to_string().contains("HP"), "got: {err}");
    }

    #[test]
    fn merge_conflict_is_exempt_when_overridden() {
        // The key both entries disagree on is in `overridden`, so no error - the
        // --env override will set it. (The P1-1 fix: --env resolves a conflict.)
        let entries = [entry("a", &[("HP", "1")]), entry("b", &[("HP", "2")])];
        let overridden: BTreeSet<&str> = ["HP"].into_iter().collect();
        let merged = merge_check_envs(
            &entries,
            &overridden,
            ActivationPurpose::Clippy.sweep_policy().env_remedy,
        )
        .unwrap();
        // The overridden key is skipped entirely here; build_clippy_sweep layers
        // the override on afterwards.
        assert!(!merged.contains_key("HP"));
    }

    // ----- build_clippy_sweep: ad-hoc -----

    #[test]
    fn check_and_clippy_adhoc_build_inputs_agree() {
        let entries = vec![entry("core", &[("CODEGEN", "1")])];
        let features = vec!["ffi".to_owned()];
        let check = decide_active_sweeps(&entries, None, None, &features, true).unwrap();
        let clippy = build_clippy_sweep(&entries, &[], false, &features, true, None, &[]).unwrap();
        assert_eq!(
            check[0].features.configured_key(),
            clippy.features.configured_key()
        );
        assert_eq!(check[0].env, clippy.env);
        assert_eq!(check[0].packages, clippy.packages);
        assert_eq!(check[0].profile, clippy.profile);
    }

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
