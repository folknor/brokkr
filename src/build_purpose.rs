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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    Dev,
    Release,
    Bench,
    /// A custom `[profile.<name>]`. Constructible only by [`profile`], which
    /// refuses it for a purpose whose policy lacks `named_profile` - so no
    /// sweep, member-check or bench request can carry one.
    Named(NamedProfile),
}

/// A custom profile [`profile`] admitted for a named-profile purpose. The
/// private field keeps every other module from minting one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamedProfile(crate::build::ProfileName);

impl NamedProfile {
    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

fn admitted(p: crate::build::RustProfile) -> Profile {
    match p {
        crate::build::RustProfile::Dev => Profile::Dev,
        crate::build::RustProfile::Release => Profile::Release,
        crate::build::RustProfile::Named(name) => Profile::Named(NamedProfile(name)),
    }
}

struct Policy {
    default_profile: Profile,
    configurable_profile: bool,
    /// Whether a custom profile is selectable: cargo build, run and install
    /// take `--profile <name>`; the sweep purposes stay dev/release.
    named_profile: bool,
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
                named_profile: true,
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
                named_profile: false,
                features: true,
                worktree_target: false,
                dev_flag: None,
                release_flag: Some("--release"),
            },
            Self::Install => Policy {
                default_profile: Profile::Release,
                configurable_profile: true,
                named_profile: true,
                features: false,
                worktree_target: false,
                dev_flag: Some("--debug"),
                release_flag: None,
            },
            Self::MemberCheck => Policy {
                default_profile: Profile::Dev,
                configurable_profile: true,
                named_profile: false,
                features: false,
                worktree_target: false,
                dev_flag: None,
                release_flag: Some("--release"),
            },
            Self::Check => Policy {
                default_profile: Profile::Dev,
                configurable_profile: true,
                named_profile: false,
                features: true,
                worktree_target: false,
                dev_flag: None,
                release_flag: Some("--release"),
            },
            Self::Bench => Policy {
                default_profile: Profile::Bench,
                configurable_profile: false,
                named_profile: false,
                features: false,
                worktree_target: true,
                dev_flag: None,
                release_flag: None,
            },
        }
    }
}

/// The profile inputs of a build, run or install: the CLI choice and the
/// command's config default. Precedence is invocation > config > purpose
/// default; no launcher implements precedence.
#[derive(Default, Clone)]
pub(crate) struct Profiles {
    pub(crate) invocation: Option<crate::build::RustProfile>,
    pub(crate) configured: Option<crate::build::RustProfile>,
}

/// Resolve a build/run/install profile. A custom profile reaching a purpose
/// that cannot take one is refused, never mapped to another profile.
pub(crate) fn profile(purpose: Purpose, inputs: Profiles) -> Result<Profile, crate::error::DevError> {
    let policy = purpose.policy();
    let requested = inputs.invocation.or(inputs.configured);
    // Checked before the fixed-profile return, so a purpose that ignores
    // profile inputs (bench) still refuses a custom one rather than
    // dropping it.
    if let Some(crate::build::RustProfile::Named(name)) = &requested
        && !policy.named_profile
    {
        return Err(crate::error::DevError::Config(format!(
            "cargo profile `{}` cannot be used here: this command builds under dev or release only",
            name.as_str()
        )));
    }
    if !policy.configurable_profile {
        return Ok(policy.default_profile);
    }
    Ok(requested.map_or(policy.default_profile, admitted))
}

/// Sweep profile inputs: the CLI `--debug`/`--release`, the `[[check]]`
/// entry's pin, and `[test] debug`. Precedence invocation > sweep pin >
/// config > purpose default. Sweeps build under dev or release only.
#[derive(Default, Clone, Copy)]
pub(crate) struct SweepProfiles {
    pub(crate) invocation: Option<bool>,
    pub(crate) sweep: Option<SweepProfile>,
    pub(crate) configured: Option<bool>,
}

impl From<SweepProfile> for Profile {
    fn from(p: SweepProfile) -> Self {
        match p {
            SweepProfile::Dev => Self::Dev,
            SweepProfile::Release => Self::Release,
        }
    }
}

/// Sweep callers can only select profiles valid for cargo check/test/clippy.
#[derive(Clone, Copy)]
pub(crate) enum SweepPurpose {
    Check,
    Test,
}

pub(crate) fn profile_for_sweep(purpose: SweepPurpose, inputs: SweepProfiles) -> SweepProfile {
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
    /// Already resolved: [`profile`] for build/run/install, the sweep's own
    /// profile for check and test, the purpose's fixed profile for bench.
    pub(crate) profile: Profile,
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

pub(crate) fn profile_args(purpose: Purpose, profile: &Profile) -> Vec<String> {
    let policy = purpose.policy();
    let flag = match profile {
        Profile::Dev => policy.dev_flag,
        Profile::Release => policy.release_flag,
        Profile::Bench => None,
        Profile::Named(name) => return vec!["--profile".into(), name.as_str().to_owned()],
    };
    flag.into_iter().map(str::to_owned).collect()
}

pub(crate) fn resolve(request: Request<'_>) -> Shape {
    let policy = request.purpose.policy();
    let mut args = Vec::new();
    args.extend(profile_args(request.purpose, &request.profile));
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
        profile: profile.into(),
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
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn profile_defaults_and_override_precedence_follow_each_contract() {
        use crate::build::RustProfile;
        for (purpose, expected) in [
            (Purpose::Run, Profile::Release),
            (Purpose::Install, Profile::Release),
            (Purpose::Test, Profile::Release),
            (Purpose::Check, Profile::Dev),
            (Purpose::Bench, Profile::Bench),
        ] {
            assert_eq!(profile(purpose, Profiles::default()).unwrap(), expected);
        }
        let corpus = RustProfile::parse("corpus").unwrap();
        assert_eq!(
            profile(
                Purpose::Run,
                Profiles {
                    invocation: Some(RustProfile::Release),
                    configured: Some(corpus.clone()),
                }
            )
            .unwrap(),
            Profile::Release
        );
        assert_eq!(
            profile(
                Purpose::Build,
                Profiles {
                    configured: Some(corpus.clone()),
                    ..Default::default()
                }
            )
            .unwrap(),
            admitted(corpus.clone())
        );
        assert!(
            profile(
                Purpose::Bench,
                Profiles {
                    configured: Some(corpus.clone()),
                    ..Default::default()
                }
            )
            .is_err(),
            "a fixed-profile purpose refuses a custom profile rather than dropping it"
        );
        assert!(
            profile(
                Purpose::Check,
                Profiles {
                    invocation: Some(corpus),
                    ..Default::default()
                }
            )
            .is_err(),
            "a custom profile must never reach a sweep purpose"
        );
        assert_eq!(
            profile_for_sweep(
                SweepPurpose::Test,
                SweepProfiles {
                    invocation: Some(false),
                    sweep: Some(SweepProfile::Dev),
                    configured: Some(true),
                }
            ),
            SweepProfile::Release
        );
        assert_eq!(
            profile_for_sweep(
                SweepPurpose::Test,
                SweepProfiles {
                    sweep: Some(SweepProfile::Release),
                    configured: Some(true),
                    ..Default::default()
                }
            ),
            SweepProfile::Release
        );
        assert_eq!(
            profile_for_sweep(
                SweepPurpose::Check,
                SweepProfiles {
                    sweep: Some(SweepProfile::Release),
                    ..Default::default()
                }
            ),
            SweepProfile::Release
        );
    }

    #[test]
    fn a_named_profile_renders_as_cargo_profile_flag() {
        let named = admitted(crate::build::RustProfile::parse("corpus").unwrap());
        for purpose in [Purpose::Build, Purpose::Run, Purpose::Install] {
            assert_eq!(profile_args(purpose, &named), ["--profile", "corpus"]);
        }
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
                profile: profile(purpose, Profiles::default()).unwrap(),
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
                profile: profile(
                    purpose,
                    Profiles {
                        invocation: Some(crate::build::RustProfile::for_debug(debug)),
                        ..Default::default()
                    },
                )
                .unwrap(),
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
