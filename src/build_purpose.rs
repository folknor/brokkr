//! Cargo build intent, shared by standalone commands and sweep planning.
//!
//! Discovery chooses targets; activation chooses sweeps; selection admits
//! packages and projects configured features. This owner turns those inputs
//! into the Cargo shape. Command differences are policy data, not launcher
//! decisions. Host `features` never enter here: measured builds receive them
//! already merged into their feature list (`command_args`), and the standalone
//! purposes (run, install, check, clippy, test, bench) do not inherit them -
//! the host feature policy in `docs/brokkr.toml.md`. Computed `[cargo_env]`
//! values remain spawn-time inputs: resolving a shape must not evaluate or
//! cache commands that the contract requires before every Cargo launch.

pub(crate) mod environment;
pub(crate) mod features;
pub(crate) mod requests;

use std::path::Path;

use crate::config::SweepProfile;
use crate::profile::PlannedSweep;

#[derive(Clone, Copy)]
pub(crate) enum Purpose {
    Run,
    Build,
    Install,
    MemberCheck,
    Check,
    Test,
    Bench,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    Dev,
    Release,
    Bench,
}

struct Policy {
    default_profile: Profile,
    configurable_profile: bool,
    features: bool,
    worktree_target: bool,
    dev_flag: Option<&'static str>,
    release_flag: Option<&'static str>,
}

impl Purpose {
    fn policy(self) -> Policy {
        // Cargo install and cargo bench have different implicit profiles from
        // cargo run/test/clippy. The flags belong to the purpose, not its caller.
        match self {
            Self::Run | Self::Build => Policy {
                default_profile: Profile::Release,
                configurable_profile: true,
                features: true,
                // `build::cargo_build` isolates a worktree's target dir itself,
                // because the same override must reach its metadata query and
                // executable lookup too; `run --commit` builds through it.
                worktree_target: false,
                dev_flag: None,
                release_flag: Some("--release"),
            },
            Self::Test => Policy {
                default_profile: Profile::Release,
                configurable_profile: true,
                features: true,
                worktree_target: false,
                dev_flag: None,
                release_flag: Some("--release"),
            },
            Self::Install => Policy {
                default_profile: Profile::Release,
                configurable_profile: true,
                features: false,
                worktree_target: false,
                dev_flag: Some("--debug"),
                release_flag: None,
            },
            Self::MemberCheck => Policy {
                default_profile: Profile::Dev,
                configurable_profile: true,
                features: false,
                worktree_target: false,
                dev_flag: None,
                release_flag: Some("--release"),
            },
            Self::Check => Policy {
                default_profile: Profile::Dev,
                configurable_profile: true,
                features: true,
                worktree_target: false,
                dev_flag: None,
                release_flag: Some("--release"),
            },
            Self::Bench => Policy {
                default_profile: Profile::Bench,
                configurable_profile: false,
                features: false,
                worktree_target: true,
                dev_flag: None,
                release_flag: None,
            },
        }
    }
}

/// Precedence is invocation > sweep pin > command config > purpose default.
/// Config defaults are optional: check/clippy have none; run/install use
/// `[bin] debug`; test uses [test] debug. No launcher implements precedence.
#[derive(Default, Clone, Copy)]
pub(crate) struct Profiles {
    pub(crate) invocation: Option<bool>,
    pub(crate) sweep: Option<SweepProfile>,
    pub(crate) configured: Option<bool>,
}

pub(crate) fn profile(purpose: Purpose, inputs: Profiles) -> Profile {
    let policy = purpose.policy();
    if !policy.configurable_profile {
        return policy.default_profile;
    }
    inputs
        .invocation
        .map(|debug| {
            if debug {
                Profile::Dev
            } else {
                Profile::Release
            }
        })
        .or_else(|| {
            inputs.sweep.map(|p| match p {
                SweepProfile::Dev => Profile::Dev,
                SweepProfile::Release => Profile::Release,
            })
        })
        .or_else(|| {
            inputs.configured.map(|debug| {
                if debug {
                    Profile::Dev
                } else {
                    Profile::Release
                }
            })
        })
        .unwrap_or(policy.default_profile)
}

/// Sweep callers can only select profiles valid for cargo check/test/clippy.
#[derive(Clone, Copy)]
pub(crate) enum SweepPurpose {
    Check,
    Test,
}

pub(crate) fn profile_for_sweep(purpose: SweepPurpose, inputs: Profiles) -> SweepProfile {
    let default = match purpose {
        SweepPurpose::Check => Purpose::Check,
        SweepPurpose::Test => Purpose::Test,
    }
    .policy()
    .default_profile;
    let from_debug = |debug| {
        if debug {
            SweepProfile::Dev
        } else {
            SweepProfile::Release
        }
    };
    inputs
        .invocation
        .map(from_debug)
        .or(inputs.sweep)
        .or_else(|| inputs.configured.map(from_debug))
        .unwrap_or_else(|| from_debug(default == Profile::Dev))
}

/// Feature values retain their caller's rendering contract. Joining values
/// changes whitespace-containing requests, so it must be an explicit choice.
#[derive(Clone, Copy)]
pub(crate) enum FeatureRendering {
    Occurrences,
    CommaJoined,
}

/// One prepared feature argv representation for every build request.
#[derive(Default)]
pub(crate) struct Features(Vec<String>);

impl Features {
    pub(crate) fn values(
        values: &[String],
        all: bool,
        no_default: bool,
        rendering: FeatureRendering,
    ) -> Self {
        let mut args = Vec::new();
        match rendering {
            FeatureRendering::Occurrences => {
                for value in values {
                    args.extend(["--features".into(), value.clone()]);
                }
                if all {
                    args.push("--all-features".into());
                }
                if no_default {
                    args.push("--no-default-features".into());
                }
            }
            FeatureRendering::CommaJoined => {
                if no_default {
                    args.push("--no-default-features".into());
                }
                if all {
                    args.push("--all-features".into());
                }
                if !values.is_empty() {
                    args.extend(["--features".into(), values.join(",")]);
                }
            }
        }
        Self(args)
    }
}

pub(crate) struct Request<'a> {
    pub(crate) purpose: Purpose,
    pub(crate) profiles: Profiles,
    /// Canonical package selection argv, produced by metadata discovery or
    /// the selection planner. The resolver never guesses workspace members.
    pub(crate) packages: Vec<String>,
    pub(crate) features: Features,
    pub(crate) unification: Vec<String>,
    pub(crate) targets: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) build_root: Option<&'a Path>,
}

pub(crate) struct Shape {
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
}

pub(crate) fn target_dir_override(build_root: &Path) -> Option<String> {
    crate::worktree::isolated_target_dir(build_root).map(|path| path.display().to_string())
}

pub(crate) fn profile_args(purpose: Purpose, profile: Profile) -> Vec<String> {
    let policy = purpose.policy();
    let flag = match profile {
        Profile::Dev => policy.dev_flag,
        Profile::Release => policy.release_flag,
        Profile::Bench => None,
    };
    flag.into_iter().map(str::to_owned).collect()
}

pub(crate) fn resolve(request: Request<'_>) -> Shape {
    let policy = request.purpose.policy();
    let profile = profile(request.purpose, request.profiles);
    let mut args = Vec::new();
    args.extend(profile_args(request.purpose, profile));
    args.extend(request.unification);
    args.extend(request.packages);
    if policy.features {
        args.extend(request.features.0);
    }
    args.extend(request.targets);
    let mut env = request.env;
    if let Some(dir) = request
        .build_root
        .filter(|_| policy.worktree_target)
        .and_then(target_dir_override)
    {
        env.retain(|(key, _)| key != "CARGO_TARGET_DIR");
        env.push(("CARGO_TARGET_DIR".into(), dir));
    }
    Shape { args, env }
}

/// All sweep compilation fragments, including support builds, use the same
/// profile/unification/package/feature resolver as standalone builds.
pub(crate) fn sweep_args(
    sweep: &PlannedSweep,
    packages: Vec<String>,
    features: &[String],
    profile: SweepProfile,
    targets: Vec<String>,
) -> Vec<String> {
    // Features arrive already projected by check's selection; they pass
    // through as argv, so the resolver does not re-render them.
    resolve(Request {
        purpose: Purpose::Check,
        profiles: Profiles {
            sweep: Some(profile),
            ..Default::default()
        },
        packages,
        features: Features(features.to_vec()),
        unification: sweep.unification_args(),
        targets,
        env: Vec::new(),
        build_root: None,
    })
    .args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_defaults_and_override_precedence_follow_each_contract() {
        for (purpose, expected) in [
            (Purpose::Run, Profile::Release),
            (Purpose::Install, Profile::Release),
            (Purpose::Test, Profile::Release),
            (Purpose::Check, Profile::Dev),
            (Purpose::Bench, Profile::Bench),
        ] {
            assert_eq!(profile(purpose, Profiles::default()), expected);
        }
        assert_eq!(
            profile(
                Purpose::Test,
                Profiles {
                    invocation: Some(false),
                    sweep: Some(SweepProfile::Dev),
                    configured: Some(true),
                }
            ),
            Profile::Release
        );
        assert_eq!(
            profile(
                Purpose::Test,
                Profiles {
                    sweep: Some(SweepProfile::Release),
                    configured: Some(true),
                    ..Default::default()
                }
            ),
            Profile::Release
        );
        assert_eq!(
            profile(
                Purpose::Check,
                Profiles {
                    sweep: Some(SweepProfile::Release),
                    ..Default::default()
                }
            ),
            Profile::Release
        );
    }

    #[test]
    fn run_preserves_feature_occurrences_and_featureless_purposes_drop_them() {
        let values = vec!["a,b c".into(), "pkg/feature".into()];
        for purpose in [
            Purpose::Run,
            Purpose::Install,
            Purpose::Check,
            Purpose::Test,
            Purpose::Bench,
        ] {
            let shape = resolve(Request {
                purpose,
                profiles: Profiles::default(),
                packages: vec!["-p".into(), "pkg".into()],
                features: Features::values(&values, false, true, FeatureRendering::Occurrences),
                unification: Vec::new(),
                targets: Vec::new(),
                env: Vec::new(),
                build_root: None,
            });
            if matches!(purpose, Purpose::Install | Purpose::Bench) {
                assert!(
                    !shape
                        .args
                        .iter()
                        .any(|a| a.starts_with("--features") || a == "--no-default-features")
                );
            } else {
                assert_eq!(
                    &shape.args[shape.args.len() - 5..],
                    [
                        "--features",
                        "a,b c",
                        "--features",
                        "pkg/feature",
                        "--no-default-features"
                    ]
                );
            }
        }
    }

    #[test]
    fn install_names_dev_while_run_names_release() {
        for (purpose, debug, expected) in [
            (Purpose::Run, true, Vec::<String>::new()),
            (Purpose::Run, false, vec!["--release".into()]),
            (Purpose::Install, true, vec!["--debug".into()]),
            (Purpose::Install, false, Vec::new()),
        ] {
            let shape = resolve(Request {
                purpose,
                profiles: Profiles {
                    invocation: Some(debug),
                    ..Default::default()
                },
                packages: Vec::new(),
                features: Features::default(),
                unification: Vec::new(),
                targets: Vec::new(),
                env: Vec::new(),
                build_root: None,
            });
            assert_eq!(shape.args, expected);
        }
    }
}
