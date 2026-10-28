// Lane eligibility, activation refusals, identities and repro rendering for check/test.
// Build requests cross into build_purpose only after lane selection is settled.

use std::fmt::{self, Display, Formatter};
use std::path::Path;

use crate::build_purpose::features::{DroppedToken, FeatureOracle};
use crate::build_purpose::requests::SupportRequest;
use crate::check_cmd::{LaneIdx, Lanes};
use crate::config::check::{CargoUnification, EffectiveUnification};
use crate::config::{Certifies, FeatureUnification, SweepProfile};
use crate::error::DevError;
use crate::profile::{LaneExec, PlannedSweep, SweepDecl};

/// Where `brokkr test`'s one package came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageSource {
    /// `-p` on the command line.
    Cli,
    /// `[test] default_package`.
    DefaultPackage,
    /// The project's built-in default package.
    ProjectDefault,
}

/// Who replaced a sweep's own selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provenance {
    /// `brokkr check -p` / `brokkr clippy -p`.
    Cli,
    /// `brokkr test`'s resolved package, whatever its source.
    ResolvedPackage(PackageSource),
}

/// The effective package selection of one phase of one sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Selection {
    /// The sweep's own `packages` list.
    Explicit(Vec<String>),
    /// `--workspace --exclude ...` from `test_exclude_packages`. Test phase
    /// only - the exclusions never narrow clippy or rustdoc.
    WorkspaceExcluding(Vec<String>),
    /// No package flags: cargo's default selection (the default members
    /// of a virtual workspace, the root package otherwise). Not the same
    /// claim as workspace coverage, and never described as one.
    Bare,
    /// An invocation-level package set REPLACING the sweep's own selection
    /// (cargo unions selection flags, so it can never combine with it).
    /// The packages are the ones the sweep's rules admitted.
    Override {
        packages: Vec<String>,
        provenance: Provenance,
    },
}

impl Selection {
    /// The packages a positive selection names; `None` for `Bare` and
    /// `WorkspaceExcluding`, which name none.
    pub(crate) fn packages(&self) -> Option<&[String]> {
        match self {
            Self::Explicit(p) | Self::Override { packages: p, .. } => Some(p),
            Self::WorkspaceExcluding(_) | Self::Bare => None,
        }
    }

    /// The same selection narrowed to one of its packages: one cargo
    /// resolution of a per-package lane. Keeps the provenance.
    pub(crate) fn narrowed_to(&self, pkg: &str) -> Self {
        match self {
            Self::Override { provenance, .. } => Self::Override {
                packages: vec![pkg.to_owned()],
                provenance: *provenance,
            },
            _ => Self::Explicit(vec![pkg.to_owned()]),
        }
    }
}

#[cfg(test)]
pub(crate) fn package_args(selection: &Selection) -> Vec<String> {
    crate::build_purpose::requests::package_args(&selection.build_selection())
}

impl Selection {
    fn build_selection(&self) -> crate::build_purpose::requests::PackageSelection {
        use crate::build_purpose::requests::PackageSelection;
        match self {
            Self::Explicit(p) | Self::Override { packages: p, .. } => {
                PackageSelection::Packages(p.clone())
            }
            Self::WorkspaceExcluding(p) => PackageSelection::WorkspaceExcluding(p.clone()),
            Self::Bare => PackageSelection::Bare,
        }
    }
}

/// Lane provenance paired with a build owner's projected Cargo request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolutionRun {
    pub(crate) resolution: Option<String>,
    pub(crate) selection: Selection,
    request: crate::build_purpose::requests::ResolutionRun,
}

impl ResolutionRun {
    fn new(
        sweep: &PlannedSweep,
        resolution: Option<String>,
        selection: Selection,
        domain: &Selection,
        oracle: &FeatureOracle<'_>,
    ) -> Result<Self, DevError> {
        let request = crate::build_purpose::requests::ResolutionRun::new(
            sweep,
            selection.build_selection(),
            &domain.build_selection(),
            oracle,
        )?;
        Ok(Self {
            resolution,
            selection,
            request,
        })
    }

    pub(crate) fn package_args(&self) -> Vec<String> {
        self.request.package_args()
    }

    pub(crate) fn feature_args(&self) -> &[String] {
        self.request.feature_args()
    }

    pub(crate) fn dropped(&self) -> &[DroppedToken] {
        self.request.dropped()
    }

    #[cfg(test)]
    pub(crate) fn cargo_args(&self) -> Vec<String> {
        self.request.cargo_args()
    }
}

/// Cargo-level arguments the invocation forwarded after `brokkr check --`
/// and libtest arguments after the optional second `--`, classified once
/// for every selection decision and runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForwardedArgs {
    cargo: Vec<String>,
    libtest: Vec<String>,
    classified: Vec<ForwardedCargoArg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ForwardedCargoArg {
    Target(Vec<String>),
    Package(Vec<String>),
    Workspace(Vec<String>),
    Unsupported(Vec<String>),
    Other(Vec<String>),
}

impl ForwardedArgs {
    pub(crate) fn parse(args: &[String]) -> Self {
        let separator = args.iter().position(|arg| arg == "--");
        let (cargo, libtest) = separator.map_or((args, &[][..]), |i| (&args[..i], &args[i + 1..]));
        Self {
            cargo: cargo.to_vec(),
            libtest: libtest.to_vec(),
            classified: classify_cargo_args(cargo),
        }
    }

    pub(crate) fn cargo(&self) -> &[String] {
        &self.cargo
    }

    pub(crate) fn libtest(&self) -> &[String] {
        &self.libtest
    }

    /// Target selectors shape enumeration and are removed from each
    /// per-binary cargo invocation. All other cargo tokens keep order.
    pub(crate) fn partition_targets(&self) -> (Vec<String>, Vec<String>) {
        let mut targets = Vec::new();
        let mut rest = Vec::new();
        for arg in &self.classified {
            let (tokens, is_target) = match arg {
                ForwardedCargoArg::Target(tokens) => (tokens, true),
                ForwardedCargoArg::Package(tokens)
                | ForwardedCargoArg::Workspace(tokens)
                | ForwardedCargoArg::Unsupported(tokens)
                | ForwardedCargoArg::Other(tokens) => (tokens, false),
            };
            if is_target {
                targets.extend(tokens.iter().cloned());
            } else {
                rest.extend(tokens.iter().cloned());
            }
        }
        (targets, rest)
    }

    pub(crate) fn has_target_selector(&self) -> bool {
        self.classified
            .iter()
            .any(|arg| matches!(arg, ForwardedCargoArg::Target(_)))
    }

    /// A package or exclusion selector narrows a bare workspace request;
    /// `--workspace` alone does not narrow it.
    pub(crate) fn narrows_package_selection(&self) -> bool {
        self.classified
            .iter()
            .any(|arg| matches!(arg, ForwardedCargoArg::Package(_)))
    }

    fn package_mode_selectors(&self) -> Vec<String> {
        self.classified
            .iter()
            .filter_map(|arg| match arg {
                ForwardedCargoArg::Package(tokens) | ForwardedCargoArg::Workspace(tokens) => {
                    Some(tokens)
                }
                _ => None,
            })
            .flatten()
            .cloned()
            .collect()
    }

    fn unsupported_direct_options(&self) -> Vec<String> {
        self.classified
            .iter()
            .filter_map(|arg| match arg {
                ForwardedCargoArg::Unsupported(tokens) => tokens.first().cloned(),
                _ => None,
            })
            .collect()
    }

    /// The forwarded `--target`, `--config`, `--manifest-path` and
    /// `--target-dir` args: each moves the platform, config chain, project or
    /// artifact root a target sweep's preflight resolved.
    pub(crate) fn platform_overrides(&self) -> Vec<String> {
        self.unsupported_direct_options()
            .into_iter()
            .filter(|token| {
                let head = token
                    .split_once('=')
                    .map_or(token.as_str(), |(head, _)| head);
                head != "--no-run"
            })
            .collect()
    }

    /// Whether the forwarded cargo args carry `--config` or `--target`
    /// (either spelling), which change the config chain or the build
    /// platform out from under anything brokkr installs into cargo.
    pub(crate) fn changes_config_or_target(&self) -> bool {
        self.classified.iter().any(|arg| match arg {
            ForwardedCargoArg::Unsupported(tokens) => tokens.first().is_some_and(|token| {
                let head = token
                    .split_once('=')
                    .map_or(token.as_str(), |(head, _)| head);
                head == "--config" || head == "--target"
            }),
            _ => false,
        })
    }

    /// Cargo argv that rides the prebuild or reproduction command after
    /// target selectors have already narrowed the plan.
    pub(crate) fn cargo_without_targets(&self) -> Vec<String> {
        self.partition_targets().1
    }
}

fn classify_cargo_args(args: &[String]) -> Vec<ForwardedCargoArg> {
    const VALUED_TARGETS: [&str; 4] = ["--test", "--bin", "--example", "--bench"];
    const BARE_TARGETS: [&str; 8] = [
        "--lib",
        "--doc",
        "--tests",
        "--bins",
        "--examples",
        "--benches",
        "--all-targets",
        "--doctests",
    ];
    let mut classified = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let head = arg.split_once('=').map_or(arg.as_str(), |(head, _)| head);
        if VALUED_TARGETS.contains(&head) {
            let tokens = if arg.contains('=') {
                vec![arg.clone()]
            } else if let Some(value) = args.get(i + 1) {
                i += 1;
                vec![arg.clone(), value.clone()]
            } else {
                vec![arg.clone()]
            };
            classified.push(ForwardedCargoArg::Target(tokens));
        } else if BARE_TARGETS.contains(&arg.as_str()) {
            classified.push(ForwardedCargoArg::Target(vec![arg.clone()]));
        } else if ["-p", "--package", "--exclude"].contains(&head) {
            let tokens = if arg.contains('=') {
                vec![arg.clone()]
            } else if let Some(value) = args.get(i + 1) {
                i += 1;
                vec![arg.clone(), value.clone()]
            } else {
                vec![arg.clone()]
            };
            classified.push(ForwardedCargoArg::Package(tokens));
        } else if arg.starts_with("-p") && !arg.starts_with("--") && arg.len() > 2 {
            classified.push(ForwardedCargoArg::Package(vec![arg.clone()]));
        } else if arg == "--workspace" {
            classified.push(ForwardedCargoArg::Workspace(vec![arg.clone()]));
        } else {
            let is_unsupported = [
                "--no-run",
                "--target",
                "--config",
                "--manifest-path",
                "--target-dir",
            ]
            .contains(&head);
            classified.push(if is_unsupported {
                ForwardedCargoArg::Unsupported(vec![arg.clone()])
            } else {
                ForwardedCargoArg::Other(vec![arg.clone()])
            });
        }
        i += 1;
    }
    classified
}

/// The one invocation-wide resolution point: package scope, forwarded
/// cargo selectors, the lazy whole-workspace metadata fact, promotion,
/// configured unification, and package-mode refusals are resolved before
/// any phase can construct a cargo request.
pub(crate) fn resolve_invocation_sweeps(
    sweeps: Vec<SweepDecl>,
    project_root: &Path,
    packages: &[String],
    forwarded: &ForwardedArgs,
) -> Result<Vec<PlannedSweep>, DevError> {
    let any_candidate = sweeps.iter().any(|sweep| {
        matches!(sweep.lane_exec(), LaneExec::Parallel { .. })
            && sweep.packages.is_empty()
            && sweep.test_exclude_packages.is_empty()
            && packages.is_empty()
            && !forwarded.narrows_package_selection()
    });
    let whole_workspace = if any_candidate {
        crate::cargo::metadata::load(
            project_root,
            crate::cargo::metadata::Query::workspace(),
            &[],
            None,
        )?
        .members_are_all_default()
    } else {
        false
    };

    sweeps
        .into_iter()
        .map(|sweep| {
            let promotion_eligible = matches!(sweep.lane_exec(), LaneExec::Parallel { .. })
                && sweep.packages.is_empty()
                && sweep.test_exclude_packages.is_empty()
                && packages.is_empty()
                && whole_workspace
                && !forwarded.narrows_package_selection();
            let planned = resolve_planned_sweep(sweep, promotion_eligible);
            reject_forwarded_package_selectors(&planned, forwarded)?;
            Ok(planned)
        })
        .collect()
}

/// Resolve the configured unification policy using the invocation's one
/// promotion answer. This is a runtime decision: package scope, forwarded
/// cargo selectors, and cargo metadata determine whether `auto` promotes.
/// An explicit `selected` remains pinned on promoted sweeps because the
/// fan-out executes the prebuilt binaries directly; it no longer needs a
/// runner-side cargo re-entry to preserve the same graph.
pub(crate) fn resolve_planned_sweep(
    declaration: SweepDecl,
    promotion_eligible: bool,
) -> PlannedSweep {
    let effective = resolve_unification(&declaration, promotion_eligible);
    PlannedSweep::with_effective_unification(declaration, effective)
}

pub(crate) fn resolve_unification(
    sweep: &SweepDecl,
    promotion_eligible: bool,
) -> EffectiveUnification {
    match sweep.feature_unification {
        FeatureUnification::Package => EffectiveUnification::Pinned(CargoUnification::Package),
        FeatureUnification::Workspace => EffectiveUnification::Pinned(CargoUnification::Workspace),
        FeatureUnification::Selected => EffectiveUnification::Pinned(CargoUnification::Selected),
        FeatureUnification::Auto if promotion_eligible => {
            EffectiveUnification::Pinned(CargoUnification::Workspace)
        }
        FeatureUnification::Auto => EffectiveUnification::Ambient,
    }
}

/// Reject forwarded package selectors that would escape a per-package
/// cargo resolution. The parsed selector set is shared with promotion.
pub(crate) fn reject_forwarded_package_selectors(
    sweep: &PlannedSweep,
    forwarded: &ForwardedArgs,
) -> Result<(), DevError> {
    if !sweep.effective_unification().is_per_package() {
        return Ok(());
    }
    let offending = forwarded.package_mode_selectors();
    if offending.is_empty() {
        return Ok(());
    }
    Err(DevError::Config(format!(
        "[[check]] '{}' runs under `feature_unification = \"package\"`, which resolves one cargo \
         invocation per package, but the forwarded args carry package selectors ({}). cargo unions \
         selection flags, so these would widen a resolution that must hold exactly one package. \
         Use brokkr's own `-p`, or the entry's `packages`.",
        sweep.label,
        offending.join(" ")
    )))
}

/// Direct execution can only honour args that shape the prebuild. These
/// flags would change the selected platform, execution policy, config, or
/// artifact roots and therefore fail before any build starts.
pub(crate) fn reject_direct_execution_args(
    sweep: &PlannedSweep,
    forwarded: &ForwardedArgs,
) -> Result<(), DevError> {
    let offending = forwarded.unsupported_direct_options();
    if offending.is_empty() {
        return Ok(());
    }
    Err(DevError::Config(format!(
        "sweep '{}' runs its test binaries directly, and the forwarded cargo args carry {} - \
         which only cargo-mediated execution can honour. Drop the flag, or configure a \
         cargo-mediated test lane.",
        sweep.label,
        offending.join(" ")
    )))
}

/// Cargo target selector for one known test binary. Library-flavoured
/// targets all map to `--lib`; the four named target kinds keep their name.
pub(crate) fn cargo_target_selector(kind: &str, target: &str) -> Vec<String> {
    match kind {
        "test" | "bin" | "example" | "bench" => vec![format!("--{kind}"), target.to_owned()],
        _ => vec!["--lib".to_owned()],
    }
}

/// The consistent, copy-pasteable cargo command rendered from a planned
/// request and one binary's target. Forwarded target selectors are already
/// reflected in the planned binary list and do not broaden the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReproLine(String);

impl ReproLine {
    pub(crate) fn parallel_binary(
        sweep: &PlannedSweep,
        binary: &crate::check_cmd::TestBinary,
        request: Option<&ResolutionRun>,
        allow_args: &[String],
        forwarded: &ForwardedArgs,
    ) -> Self {
        let mut args = vec!["test".to_owned()];
        args.extend(allow_args.iter().cloned());
        if let Some(request) = request {
            // The planned request through the one selection renderer;
            // the binary narrows only the target.
            args.extend(crate::check_cmd::cargo_selection_args(
                sweep,
                request,
                sweep.cargo_profile(),
                crate::check_cmd::CargoSelectionPurpose::Shape,
            ));
        } else {
            args.extend(crate::build_purpose::sweep_args(
                sweep,
                vec!["-p".to_owned(), binary.package.clone()],
                &[],
                sweep.cargo_profile(),
                Vec::new(),
            ));
        }
        args.extend(cargo_target_selector(
            binary.selector_kind(),
            &binary.target,
        ));
        args.extend(forwarded.cargo_without_targets());
        let mut libtest = sweep.lane_filters().libtest_args();
        libtest.extend(sweep.lane_filters().name_filters());
        if !libtest.is_empty() {
            args.push("--".into());
            args.extend(libtest);
        }
        Self(format!("failing command: cargo {}", args.join(" ")))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A versioned, length-prefixed encoder for identity keys. Fields are
/// encoded in declaration order and enums use explicit tags; changing a
/// Rust `Debug` implementation therefore cannot silently change identity.
#[derive(Default)]
struct StableEncoder(Vec<u8>);

impl StableEncoder {
    fn tag(&mut self, value: u8) {
        self.0.push(value);
    }

    fn count(&mut self, value: usize) {
        self.0.extend_from_slice(&(value as u64).to_be_bytes());
    }

    fn string(&mut self, value: &str) {
        self.count(value.len());
        self.0.extend_from_slice(value.as_bytes());
    }

    fn strings(&mut self, values: &[String]) {
        self.count(values.len());
        for value in values {
            self.string(value);
        }
    }

    fn pairs(&mut self, values: &[(String, String)]) {
        self.count(values.len());
        for (key, value) in values {
            self.string(key);
            self.string(value);
        }
    }

    fn profile(&mut self, value: Option<SweepProfile>) {
        match value {
            None => self.tag(0),
            Some(SweepProfile::Dev) => self.tag(1),
            Some(SweepProfile::Release) => self.tag(2),
        }
    }

    /// A sweep's target triple, appended last and only when set, so every
    /// host shape keeps the encoding (and so the id) it had before the key
    /// existed.
    fn target(&mut self, value: Option<&str>) {
        if let Some(triple) = value {
            self.string("target");
            self.string(triple);
        }
    }

    fn unification(&mut self, value: EffectiveUnification) {
        match value.describe() {
            None => self.tag(0),
            Some(mode) => {
                self.tag(1);
                self.string(mode);
            }
        }
    }

    fn finish(self) -> Vec<u8> {
        self.0
    }
}

/// The configured package/feature shape used to group policy universes.
/// The test package exclusions are included because they change the
/// enumerated universe. Support binaries are omitted: they are execution
/// inputs, not part of the set of test pairs a shape can contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclaredShape {
    packages: Vec<String>,
    test_exclude_packages: Vec<String>,
    features: Vec<String>,
    rustflags: Vec<String>,
    env: Vec<(String, String)>,
    profile: Option<SweepProfile>,
    unification: EffectiveUnification,
    target: Option<String>,
}

impl DeclaredShape {
    pub(crate) fn of(sweep: &PlannedSweep) -> Self {
        Self {
            packages: sweep.packages.clone(),
            test_exclude_packages: sweep.test_exclude_packages.clone(),
            features: sweep.features.configured_key(),
            rustflags: sweep.rustflags.clone(),
            env: sweep
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            profile: sweep.profile,
            unification: sweep.effective_unification(),
            target: sweep.target.clone(),
        }
    }

    fn stable_encoding(&self) -> Vec<u8> {
        let mut out = StableEncoder::default();
        out.string("brokkr.declared-shape.v1");
        out.strings(&self.packages);
        out.strings(&self.test_exclude_packages);
        out.strings(&self.features);
        out.strings(&self.rustflags);
        out.pairs(&self.env);
        out.profile(self.profile);
        out.unification(self.unification);
        out.target(self.target.as_deref());
        out.finish()
    }

    pub(crate) fn id(&self) -> ShapeId {
        let encoding = self.stable_encoding();
        ShapeId(format!("{:016x}", xxhash_rust::xxh3::xxh3_64(&encoding)))
    }
}

/// Stable text form of one declared policy shape. It is minted only from
/// `DeclaredShape`, never from an effective request or a debug rendering.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub(crate) struct ShapeId(String);

impl From<String> for ShapeId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ShapeId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl Display for ShapeId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The phases a per-sweep selection exists for. Preparation reads Test's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionPhase {
    Clippy,
    Rustdoc,
    Test,
}

impl SelectionPhase {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Clippy => "clippy",
            Self::Rustdoc => "rustdoc",
            Self::Test => "test",
        }
    }

    /// `test_exclude_packages` narrows the test invocation only: clippy
    /// and rustdoc stay workspace-wide.
    fn honours_test_exclusions(self) -> bool {
        self == Self::Test
    }
}

/// How many cargo resolutions a lane's selection is run as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolutions {
    /// One resolution over the whole selection.
    Combined,
    /// One resolution per package (`feature_unification = "package"`):
    /// a batched multi-`-p` run resolves a graph none of them install
    /// under. Always EQUAL to the selection's package list - same set,
    /// same order, no duplicates.
    PerPackage(Vec<String>),
}

impl Resolutions {
    /// The only constructor: derived from the selection, so it cannot
    /// disagree with it. Package mode over a selection that names no
    /// packages is a construction error, never repaired into one
    /// unscoped resolution.
    fn for_selection(
        sweep: &PlannedSweep,
        selection: &Selection,
        phase: SelectionPhase,
    ) -> Result<Self, DevError> {
        if let Some(p) = selection.packages()
            && has_duplicates(p)
        {
            return Err(DevError::Config(format!(
                "sweep '{}': its {} selection names a package twice ({}) - a brokkr bug, \
                 please report the `[[check]]` entry",
                sweep.label,
                phase.name(),
                p.join(", ")
            )));
        }
        if !sweep.effective_unification().is_per_package() {
            return Ok(Self::Combined);
        }
        match selection.packages() {
            Some(p) => Ok(Self::PerPackage(p.to_vec())),
            None => Err(DevError::Config(format!(
                "sweep '{}' resolves once per package (`feature_unification = \"package\"`), \
                 but its {} selection names no packages; package mode needs an explicit \
                 `packages` list",
                sweep.label,
                phase.name()
            ))),
        }
    }
}

/// A selection with its provenance dropped: what decides what cargo
/// selects, and nothing about who asked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectedSelection {
    Packages(Vec<String>),
    WorkspaceExcluding(Vec<String>),
    Bare,
}

impl ProjectedSelection {
    fn of(selection: &Selection) -> Self {
        match selection {
            Selection::Explicit(p) | Selection::Override { packages: p, .. } => {
                Self::Packages(p.clone())
            }
            Selection::WorkspaceExcluding(x) => Self::WorkspaceExcluding(x.clone()),
            Selection::Bare => Self::Bare,
        }
    }

    fn encode(&self, out: &mut StableEncoder) {
        match self {
            Self::Packages(packages) => {
                out.tag(1);
                out.strings(packages);
            }
            Self::WorkspaceExcluding(packages) => {
                out.tag(2);
                out.strings(packages);
            }
            Self::Bare => out.tag(3),
        }
    }
}

/// A package name inside a build-identity key, where its type must not be
/// confused with a feature, flag, path, or test filter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PkgName(String);

impl PkgName {
    fn encode(&self, out: &mut StableEncoder) {
        out.string(&self.0);
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

/// Resolution boundary for one request in an effective build.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Resolution {
    Combined,
    Package(PkgName),
}

impl serde::Serialize for Resolution {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Combined => serializer.serialize_none(),
            Self::Package(package) => serializer.serialize_some(package.as_str()),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Resolution {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::from_option(value.as_deref()))
    }
}

impl Resolution {
    pub(crate) fn from_option(value: Option<&str>) -> Self {
        match value {
            Some(package) => Self::Package(PkgName(package.to_owned())),
            None => Self::Combined,
        }
    }

    pub(crate) fn as_option(&self) -> Option<&str> {
        match self {
            Self::Combined => None,
            Self::Package(package) => Some(package.as_str()),
        }
    }

    fn encode(&self, out: &mut StableEncoder) {
        match self {
            Self::Combined => out.tag(0),
            Self::Package(package) => {
                out.tag(1);
                package.encode(out);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EffectiveBuildRequest {
    selection: ProjectedSelection,
    resolution: Resolution,
    features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SupportBuild {
    package: PkgName,
    features: Vec<String>,
}

/// What a lane effectively builds: compile settings plus every concrete
/// cargo request, with selection provenance removed. The named request
/// types and versioned encoding keep field roles and identity explicit.
#[derive(Debug, Clone)]
struct EffectiveBuild {
    profile: Option<SweepProfile>,
    target: Option<String>,
    rustflags: Vec<String>,
    env: Vec<(String, String)>,
    unification: EffectiveUnification,
    runs: Vec<EffectiveBuildRequest>,
    support: Vec<SupportBuild>,
}

impl EffectiveBuild {
    fn stable_encoding(&self) -> Vec<u8> {
        let mut out = StableEncoder::default();
        out.string("brokkr.effective-build.v1");
        out.profile(self.profile);
        out.strings(&self.rustflags);
        out.pairs(&self.env);
        out.unification(self.unification);
        out.count(self.runs.len());
        for run in &self.runs {
            run.selection.encode(&mut out);
            run.resolution.encode(&mut out);
            out.strings(&run.features);
        }
        out.count(self.support.len());
        for support in &self.support {
            support.package.encode(&mut out);
            out.strings(&support.features);
        }
        out.target(self.target.as_deref());
        out.finish()
    }
}

impl PartialEq for EffectiveBuild {
    fn eq(&self, other: &Self) -> bool {
        // The stable encoding is the identity contract: new fields that
        // affect compilation must be added to `stable_encoding`.
        self.stable_encoding() == other.stable_encoding()
    }
}

impl Eq for EffectiveBuild {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportIdentity {
    Exclude,
    Include,
}

/// Which package rule ruled a `-p` package out of a sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionRule {
    /// The sweep's `test_exclude_packages` names it (test phase only).
    TestExcluded,
    /// The sweep has a `packages` list and it is not on it.
    NotListed,
    /// The sweep selects test targets (`tests`, `lib_tests`) and the package
    /// has none of them. Only a focused `brokkr test` run projects selection
    /// onto its package; `check -p` hands cargo the selection as configured.
    NoSelectedTarget,
}

/// A package the invocation named and a sweep's rules did not admit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdmissionNote {
    pub(crate) package: String,
    pub(crate) rule: AdmissionRule,
}

impl std::fmt::Display for AdmissionNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.rule {
            AdmissionRule::TestExcluded => {
                write!(
                    f,
                    "-p {} is in this sweep's test_exclude_packages",
                    self.package
                )
            }
            AdmissionRule::NotListed => {
                write!(
                    f,
                    "-p {} is not in this sweep's packages list",
                    self.package
                )
            }
            AdmissionRule::NoSelectedTarget => {
                write!(
                    f,
                    "-p {} has none of the test targets this sweep selects",
                    self.package
                )
            }
        }
    }
}

/// What a sweep's rules made of the invocation's package set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Admission {
    /// The admitted packages, in invocation order, each once.
    pub(crate) kept: Vec<String>,
    /// One note per package not admitted, each once.
    pub(crate) notes: Vec<AdmissionNote>,
}

/// The package rules. The exclusion list is checked before the packages
/// list, so a package both rule out is reported as excluded.
fn admit(sweep: &PlannedSweep, packages: &[String], phase: SelectionPhase) -> Admission {
    let excluded: &[String] = if phase.honours_test_exclusions() {
        &sweep.test_exclude_packages
    } else {
        &[]
    };
    let allowlist = (!sweep.packages.is_empty())
        .then_some((sweep.packages.as_slice(), AdmissionRule::NotListed));
    admit_against(packages, excluded, allowlist)
}

/// The one admission rule every selection shares: `excluded` rules a
/// package out first, then a package off `allowlist` (when there is one) is
/// ruled out under the paired rule. Kept packages are in `packages` order,
/// each once; every refused package gets one note.
fn admit_against(
    packages: &[String],
    excluded: &[String],
    allowlist: Option<(&[String], AdmissionRule)>,
) -> Admission {
    let mut out = Admission::default();
    for pkg in packages {
        let rule = if excluded.contains(pkg) {
            Some(AdmissionRule::TestExcluded)
        } else {
            allowlist.and_then(|(list, rule)| (!list.contains(pkg)).then_some(rule))
        };
        match rule {
            Some(rule) => {
                let note = AdmissionNote {
                    package: pkg.clone(),
                    rule,
                };
                if !out.notes.contains(&note) {
                    out.notes.push(note);
                }
            }
            None => {
                if !out.kept.contains(pkg) {
                    out.kept.push(pkg.clone());
                }
            }
        }
    }
    out
}

/// A lane the phase is eligible to attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attempt {
    selection: Selection,
    resolutions: Resolutions,
    notes: Vec<AdmissionNote>,
    /// One request per resolution, in order - constructed once, with the
    /// selection, never recreated on demand.
    runs: Vec<ResolutionRun>,
    /// One request per `build_packages` entry, in order.
    support: Vec<SupportRequest>,
    /// The feature argv the lane carries in at least one run: what a
    /// lane-level description renders.
    features: Vec<String>,
}

impl Attempt {
    /// Build an attempt and its requests. `domain` is the sweep's
    /// configured selection for the phase (before any override or
    /// per-package split): what its feature tokens were written against.
    fn build(
        sweep: &PlannedSweep,
        selection: Selection,
        resolutions: Resolutions,
        notes: Vec<AdmissionNote>,
        domain: &Selection,
        oracle: &FeatureOracle<'_>,
    ) -> Result<Self, DevError> {
        let selections: Vec<(Option<String>, Selection)> = match &resolutions {
            Resolutions::Combined => vec![(None, selection.clone())],
            Resolutions::PerPackage(packages) => packages
                .iter()
                .map(|p| (Some(p.clone()), selection.narrowed_to(p)))
                .collect(),
        };
        let mut runs = Vec::with_capacity(selections.len());
        for (resolution, sel) in selections {
            runs.push(ResolutionRun::new(sweep, resolution, sel, domain, oracle)?);
        }
        let mut support = Vec::with_capacity(sweep.build_packages.len());
        for pkg in &sweep.build_packages {
            support.push(SupportRequest::new(
                sweep,
                pkg.clone(),
                &domain.build_selection(),
                oracle,
            )?);
        }
        let cfg = &sweep.features;
        let carried: Vec<String> = cfg
            .tokens
            .iter()
            .filter(|t| {
                runs.iter()
                    .any(|r| !r.dropped().iter().any(|d| &d.token == *t))
            })
            .cloned()
            .collect();
        let features = cfg.argv_with(&carried);
        Ok(Self {
            selection,
            resolutions,
            notes,
            runs,
            support,
            features,
        })
    }

    pub(crate) fn selection(&self) -> &Selection {
        &self.selection
    }

    /// The support pre-builds, in `build_packages` order.
    pub(crate) fn support(&self) -> &[SupportRequest] {
        &self.support
    }

    /// The one request of a lane that runs a single combined resolution;
    /// `None` under package mode.
    pub(crate) fn combined_run(&self) -> Option<&ResolutionRun> {
        match (&self.resolutions, self.runs.as_slice()) {
            (Resolutions::Combined, [one]) => Some(one),
            _ => None,
        }
    }

    /// The feature argv the lane carries in at least one of its runs -
    /// for a lane-level description, never a cargo argv.
    pub(crate) fn described_features(&self) -> &[String] {
        &self.features
    }

    /// The effective key of what this lane compiles: everything
    /// compile-affecting about the sweep plus every request, provenance
    /// excluded. `profile` is `None` where the caller substitutes its own
    /// answer (`brokkr test`'s effective debug).
    fn effective_build(
        &self,
        sweep: &PlannedSweep,
        profile: Option<SweepProfile>,
        support_identity: SupportIdentity,
    ) -> EffectiveBuild {
        EffectiveBuild {
            profile,
            target: sweep.target.clone(),
            rustflags: sweep.rustflags.clone(),
            env: sweep
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            unification: sweep.effective_unification(),
            runs: self
                .runs
                .iter()
                .map(|r| EffectiveBuildRequest {
                    selection: ProjectedSelection::of(&r.selection),
                    resolution: Resolution::from_option(r.resolution.as_deref()),
                    features: r.feature_args().to_vec(),
                })
                .collect(),
            support: match support_identity {
                SupportIdentity::Exclude => Vec::new(),
                SupportIdentity::Include => self
                    .support
                    .iter()
                    .map(|s| SupportBuild {
                        package: PkgName(s.package().to_owned()),
                        features: s.feature_args().to_vec(),
                    })
                    .collect(),
            },
        }
    }

    /// Consumers iterate [`Self::runs`]; this is the invariant's witness.
    #[cfg(test)]
    pub(crate) fn resolutions(&self) -> &Resolutions {
        &self.resolutions
    }

    /// The invocation packages this sweep's rules did not admit.
    pub(crate) fn notes(&self) -> &[AdmissionNote] {
        &self.notes
    }

    /// The cargo resolutions this lane runs as, in order.
    pub(crate) fn runs(&self) -> &[ResolutionRun] {
        &self.runs
    }

    /// The request of the resolution `resolution` names.
    pub(crate) fn run_for(&self, resolution: Option<&str>) -> Option<&ResolutionRun> {
        self.runs
            .iter()
            .find(|r| r.resolution.as_deref() == resolution)
    }
}

/// A lane whose rules admit none of the invocation's packages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Excluded {
    notes: Vec<AdmissionNote>,
}

impl Excluded {
    pub(crate) fn notes(&self) -> &[AdmissionNote] {
        &self.notes
    }

    /// Every note, joined: why the lane is not attempted.
    pub(crate) fn reason(&self) -> String {
        join_notes(&self.notes)
    }
}

/// A lane whose work an EARLIER attempted lane of the same phase already
/// covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Deduped {
    onto: LaneIdx,
    selection: Selection,
    notes: Vec<AdmissionNote>,
    /// The tokens this lane's own requests dropped, kept although the
    /// requests themselves are not: a lane folded onto another still
    /// narrowed its own features, and that is announced like any other.
    drops: Vec<(String, DroppedToken)>,
}

impl Deduped {
    /// The index of the earlier attempted lane.
    pub(crate) fn onto(&self) -> LaneIdx {
        self.onto
    }

    pub(crate) fn selection(&self) -> &Selection {
        &self.selection
    }

    pub(crate) fn notes(&self) -> &[AdmissionNote] {
        &self.notes
    }
}

/// A lane this phase does not apply to at all (rustdoc's doctest
/// carriers), with what the rules made of the invocation regardless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotApplicable {
    reason: &'static str,
    admission: Admission,
}

impl NotApplicable {
    pub(crate) fn reason(&self) -> &'static str {
        self.reason
    }

    pub(crate) fn admission(&self) -> &Admission {
        &self.admission
    }
}

/// One sweep's disposition in one phase. An enum by disposition, so a
/// pairing that makes no sense (a deduped lane with resolutions, an
/// excluded lane with a selection) cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaneEntry {
    /// The phase does not run: no admission was computed.
    Disabled(&'static str),
    Excluded(Excluded),
    Attempt(Attempt),
    Deduped(Deduped),
    NotApplicable(NotApplicable),
}

impl LaneEntry {
    pub(crate) fn attempt(&self) -> Option<&Attempt> {
        match self {
            Self::Attempt(a) => Some(a),
            _ => None,
        }
    }

    /// The invocation packages this sweep's rules did not admit, whatever
    /// its disposition.
    pub(crate) fn notes(&self) -> &[AdmissionNote] {
        match self {
            Self::Disabled(_) => &[],
            Self::Excluded(e) => e.notes(),
            Self::Attempt(a) => a.notes(),
            Self::Deduped(d) => d.notes(),
            Self::NotApplicable(n) => &n.admission().notes,
        }
    }

    /// Why this lane is not attempted; `None` for an attempt.
    pub(crate) fn skip_reason(&self) -> Option<String> {
        match self {
            Self::Attempt(_) => None,
            Self::Disabled(r) | Self::NotApplicable(NotApplicable { reason: r, .. }) => {
                Some((*r).to_owned())
            }
            Self::Excluded(e) => Some(e.reason()),
            Self::Deduped(d) => Some(format!("deduped onto lane {}", d.onto.get())),
        }
    }
}

const PHASE_SKIPPED: &str = "the phase does not run";
const DOCTEST_CARRIER: &str = "doctest carrier, no build shape of its own";
const DERIVED_CARRIER: &str = "derived doctest carrier, linted with the lane it was derived from";

/// The doctest carriers a `certifies = "complete"` run adds: one per tested
/// build its configured lanes leave without one.
///
/// The obligation is per tested build, not per profile: a doctest under
/// `cfg(feature = "ffi")` compiles only in a build with that feature, so one
/// workspace doc twin cannot stand in for a package-scoped feature lane. A
/// build is identified by the test phase's [`EffectiveBuild`] with support
/// builds included - every concrete request (selection, resolution boundary,
/// projected features), unification, profile, target, rustflags, env and the
/// `build_packages` a doctest can run - which is exactly what a clone of the
/// lane with doc-only execution builds, since a complete claim refuses `-p`
/// and every request comes from the lane's own configuration.
///
/// A configured `doc_only` lane discharges the build it equals. Every other
/// non-curated lane whose resolutions reach a doctested member and whose build
/// no carrier holds yet gets [`PlannedSweep::doctest_carrier`]; equal builds
/// collapse onto the first. A curated lane runs a hand-picked subset by
/// declaration and owes nothing on its own.
///
/// `reserved` holds labels a carrier may not take beyond the tested lanes'
/// own: the run's lint lanes, which are no part of the obligation and so are
/// kept out of `sweeps`.
pub(crate) fn derive_doctest_carriers(
    sweeps: &[PlannedSweep],
    reserved: &[&str],
    facts: &crate::check_cmd::DocFacts,
    oracle: &FeatureOracle<'_>,
) -> Result<Vec<PlannedSweep>, DevError> {
    let test = PhaseSelection::for_check(SelectionPhase::Test, sweeps, &[], oracle)?;
    let key = |i: usize, sweep: &PlannedSweep| {
        test.attempt(LaneIdx::new(i))
            .map(|a| a.effective_build(sweep, sweep.profile, SupportIdentity::Include))
    };
    let mut carried: Vec<EffectiveBuild> = sweeps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.lane_exec().is_doc_only())
        .filter_map(|(i, s)| key(i, s))
        .collect();
    let mut labels: Vec<String> = sweeps
        .iter()
        .map(|s| s.label.clone())
        .chain(reserved.iter().map(|r| (*r).to_owned()))
        .collect();
    let mut out = Vec::new();
    for (i, sweep) in sweeps.iter().enumerate() {
        if sweep.lane_exec().is_doc_only() || sweep.curated {
            continue;
        }
        let Some(attempt) = test.attempt(LaneIdx::new(i)) else {
            continue;
        };
        if !attempt
            .runs()
            .iter()
            .any(|run| facts.obliges(&run.selection))
        {
            continue;
        }
        let build = attempt.effective_build(sweep, sweep.profile, SupportIdentity::Include);
        if carried.contains(&build) {
            continue;
        }
        carried.push(build);
        // The display label only: identity is the typed origin. Kept unique
        // because preparation hands builds over by label.
        let mut label = format!("{} (doctests)", sweep.label);
        let mut n = 2;
        while labels.contains(&label) {
            label = format!("{} (doctests {n})", sweep.label);
            n += 1;
        }
        labels.push(label.clone());
        out.push(sweep.doctest_carrier(label));
    }
    Ok(out)
}

/// What a phase dedupes on.
enum Dedupe<'a> {
    /// `check`'s test lanes never dedupe: their filters and execution
    /// policies differ even where their builds do not.
    Never,
    /// Clippy and rustdoc: two lanes compiling the same requests under
    /// the same compile inputs ([`EffectiveBuild`]) are one lint (or doc)
    /// surface.
    BuildShape,
    /// `brokkr test`: two lanes that would execute identically
    /// ([`ExecProjection`]), with each sweep's effective debug answer.
    Execution(&'a dyn Fn(&PlannedSweep) -> bool),
}

/// A selection refusal with structured fields; its displayed wording is
/// the user-facing message passed to the existing command error path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectionRefusal {
    NothingReached {
        phase: SelectionPhase,
        packages: Vec<String>,
        applicable: Vec<String>,
    },
}

impl Display for SelectionRefusal {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::NothingReached {
                phase: SelectionPhase::Test,
                packages,
                ..
            } => write!(
                f,
                "-p {}: every sweep's config rules the selection out; zero tests ran",
                packages.join(" -p ")
            ),
            Self::NothingReached {
                phase,
                packages,
                applicable,
            } => write!(
                f,
                "-p {}: every sweep's config rules the selection out ({}); nothing reached {}",
                packages.join(" -p "),
                applicable.join(", "),
                phase.name()
            ),
        }
    }
}

/// One phase's selection: one entry per active sweep, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhaseSelection {
    phase: SelectionPhase,
    enabled: bool,
    /// The invocation's override package set (CLI `-p`, or `brokkr test`'s
    /// package); empty when the sweeps' own selections stand.
    overrides: Vec<String>,
    entries: Lanes<LaneEntry>,
    /// "Nothing reached this phase" - stored here at construction and
    /// reported at the phase boundary, never raised here.
    refusal: Option<SelectionRefusal>,
}

impl PhaseSelection {
    /// A phase that does not run: every entry `Disabled`, nothing
    /// validated, no refusal.
    pub(crate) fn disabled(
        phase: SelectionPhase,
        sweeps: &[PlannedSweep],
        reason: &'static str,
    ) -> Self {
        Self {
            phase,
            enabled: false,
            overrides: Vec::new(),
            entries: Lanes::from_sweeps(sweeps, |_, _| LaneEntry::Disabled(reason)),
            refusal: None,
        }
    }

    /// `brokkr check`'s (and `brokkr clippy`'s) selection for one enabled
    /// phase. `cli` is the CLI `-p` set; empty leaves every sweep's own
    /// selection standing.
    pub(crate) fn for_check(
        phase: SelectionPhase,
        sweeps: &[PlannedSweep],
        cli: &[String],
        oracle: &FeatureOracle<'_>,
    ) -> Result<Self, DevError> {
        let dedupe = match phase {
            SelectionPhase::Test => Dedupe::Never,
            SelectionPhase::Clippy | SelectionPhase::Rustdoc => Dedupe::BuildShape,
        };
        let provenance = (!cli.is_empty()).then_some(Provenance::Cli);
        let mut selection = Self::construct(phase, sweeps, cli, provenance, &[], &dedupe, oracle)?;
        selection.refusal = selection.nothing_reached(sweeps);
        Ok(selection)
    }

    /// `brokkr test`'s selection: its resolved package ALWAYS overrides,
    /// whatever its source, and lanes that would execute identically
    /// dedupe ([`ExecProjection`]). `debug_of` is each sweep's effective
    /// debug answer (`resolve_debug`). Apply `--sweep` before this.
    /// `untargeted[i]` marks a sweep whose target selection, projected onto
    /// `package`, selects nothing: it is excluded, ahead of the dedupe and of
    /// any support build, rather than run with no selector - which would
    /// select every test target.
    pub(crate) fn for_brokkr_test(
        sweeps: &[PlannedSweep],
        package: &str,
        source: PackageSource,
        untargeted: &[bool],
        debug_of: &dyn Fn(&PlannedSweep) -> bool,
        oracle: &FeatureOracle<'_>,
    ) -> Result<Self, DevError> {
        Self::construct(
            SelectionPhase::Test,
            sweeps,
            &[package.to_owned()],
            Some(Provenance::ResolvedPackage(source)),
            untargeted,
            &Dedupe::Execution(debug_of),
            oracle,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn construct(
        phase: SelectionPhase,
        sweeps: &[PlannedSweep],
        overrides: &[String],
        provenance: Option<Provenance>,
        untargeted: &[bool],
        dedupe: &Dedupe<'_>,
        oracle: &FeatureOracle<'_>,
    ) -> Result<Self, DevError> {
        let mut shapes: Vec<(EffectiveBuild, LaneIdx)> = Vec::new();
        let mut projections: Vec<(ExecProjection, LaneIdx)> = Vec::new();
        let entries = Lanes::try_from_sweeps(sweeps, |i, sweep| -> Result<LaneEntry, DevError> {
            let admission = match provenance {
                Some(_) => admit(sweep, overrides, phase),
                None => Admission::default(),
            };
            if untargeted.get(i.get()).copied().unwrap_or(false) && !admission.kept.is_empty() {
                return Ok(LaneEntry::Excluded(Excluded {
                    notes: admission
                        .kept
                        .iter()
                        .map(|package| AdmissionNote {
                            package: package.clone(),
                            rule: AdmissionRule::NoSelectedTarget,
                        })
                        .collect(),
                }));
            }
            if phase == SelectionPhase::Rustdoc
                && matches!(sweep.lane_exec(), crate::profile::LaneExec::DocOnly { .. })
            {
                return Ok(LaneEntry::NotApplicable(NotApplicable {
                    reason: DOCTEST_CARRIER,
                    admission,
                }));
            }
            // A derived carrier compiles exactly what its source lane does,
            // which that lane's own entry already lints.
            if phase != SelectionPhase::Test && sweep.origin.is_derived_carrier() {
                return Ok(LaneEntry::NotApplicable(NotApplicable {
                    reason: DERIVED_CARRIER,
                    admission,
                }));
            }
            let Admission { kept, notes } = admission;
            // The domain the sweep's feature tokens were written against:
            // its own selection for this phase, before any override or
            // per-package split.
            let configured = Selection::configured(sweep, phase);
            let selection = match provenance {
                Some(_) if kept.is_empty() => {
                    return Ok(LaneEntry::Excluded(Excluded { notes }));
                }
                Some(provenance) => Selection::Override {
                    packages: kept,
                    provenance,
                },
                None => configured.clone(),
            };
            // Validated for every lane that has a selection, deduped ones
            // included: an invalid pairing is never repaired, nor hidden.
            let resolutions = Resolutions::for_selection(sweep, &selection, phase)?;
            let attempt =
                Attempt::build(sweep, selection, resolutions, notes, &configured, oracle)?;
            let onto = match dedupe {
                Dedupe::Never => None,
                Dedupe::BuildShape => {
                    let build =
                        attempt.effective_build(sweep, sweep.profile, SupportIdentity::Exclude);
                    let onto = shapes
                        .iter()
                        .find(|(candidate, _)| *candidate == build)
                        .map(|(_, j)| *j);
                    if onto.is_none() {
                        shapes.push((build, i));
                    }
                    onto
                }
                Dedupe::Execution(debug_of) => {
                    let p = ExecProjection::of(sweep, &attempt, debug_of(sweep));
                    let onto = projections.iter().find(|(k, _)| *k == p).map(|(_, j)| *j);
                    if onto.is_none() {
                        projections.push((p, i));
                    }
                    onto
                }
            };
            Ok(match onto {
                Some(onto) => {
                    let drops = attempt.drop_records();
                    LaneEntry::Deduped(Deduped {
                        onto,
                        selection: attempt.selection,
                        notes: attempt.notes,
                        drops,
                    })
                }
                None => LaneEntry::Attempt(attempt),
            })
        })?;
        Ok(Self {
            phase,
            enabled: true,
            overrides: overrides.to_vec(),
            entries,
            refusal: None,
        })
    }

    /// The refusal of a run whose CLI `-p` rules every applicable sweep
    /// out of this phase: nothing would be checked, which must not read as
    /// clean. A phase with no applicable sweep at all (rustdoc over only
    /// doctest carriers) is not refused.
    fn nothing_reached(&self, sweeps: &[PlannedSweep]) -> Option<SelectionRefusal> {
        if !self.enabled || self.overrides.is_empty() {
            return None;
        }
        if self
            .entries
            .iter()
            .any(|e| matches!(e, LaneEntry::Attempt(_)))
        {
            return None;
        }
        let applicable: Vec<&str> = self
            .entries
            .iter_with(sweeps)
            .filter(|(_, _, e)| !matches!(e, LaneEntry::NotApplicable(_) | LaneEntry::Disabled(_)))
            .map(|(_, s, _)| s.label.as_str())
            .collect();
        if applicable.is_empty() {
            return None;
        }
        Some(SelectionRefusal::NothingReached {
            phase: self.phase,
            packages: self.overrides.clone(),
            applicable: applicable.into_iter().map(str::to_owned).collect(),
        })
    }

    pub(crate) fn phase(&self) -> SelectionPhase {
        self.phase
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The invocation's override package set; empty when none.
    pub(crate) fn overrides(&self) -> &[String] {
        &self.overrides
    }

    pub(crate) fn entries(&self) -> &Lanes<LaneEntry> {
        &self.entries
    }

    pub(crate) fn entry(&self, sweep: LaneIdx) -> Option<&LaneEntry> {
        self.entries.get(sweep)
    }

    pub(crate) fn attempt(&self, sweep: LaneIdx) -> Option<&Attempt> {
        self.entry(sweep).and_then(LaneEntry::attempt)
    }

    pub(crate) fn refusal(&self) -> Option<&SelectionRefusal> {
        self.refusal.as_ref()
    }

    /// Report the stored refusal, at the phase boundary.
    pub(crate) fn check_refusal(&self) -> Result<(), DevError> {
        match &self.refusal {
            Some(refusal) => Err(DevError::Config(refusal.to_string())),
            None => Ok(()),
        }
    }
}

/// What `brokkr test` dedupes on: everything that decides what a lane
/// builds and executes, and nothing about where its selection came from.
///
/// The lane's [`EffectiveBuild`] - every run request (effective selection
/// WITHOUT provenance, resolution boundary, projected features) and
/// support request, unification, rustflags, env - with the effective
/// debug answer in place of the configured profile (which `--debug`/
/// `--release` can override), and `doc_only` - a sweep and its doctest
/// twin run different things. Admission notes are outside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecProjection {
    key: EffectiveBuild,
    debug: bool,
    doc_only: bool,
    /// The (projected) target selection and exclusions: two sweeps of one
    /// build that select different harnesses search different tests, and
    /// folding one onto the other would lose a test only the second holds.
    targets: Vec<crate::profile::TargetSelector>,
    exclude: Vec<crate::profile::TargetName>,
}

impl ExecProjection {
    fn of(sweep: &PlannedSweep, attempt: &Attempt, debug: bool) -> Self {
        Self {
            key: attempt.effective_build(sweep, None, SupportIdentity::Include),
            debug,
            doc_only: matches!(sweep.lane_exec(), crate::profile::LaneExec::DocOnly { .. }),
            targets: sweep.lane_filters().targets.clone(),
            exclude: sweep.lane_filters().exclude.clone(),
        }
    }
}

/// One line per dropped token per lane: the run (or support build) that
/// does not carry it, and the members that route it. Rendered once per
/// lane however many phases share the request - the same text from two
/// phases is one line. Each selection comes with the lanes it was built over:
/// a `check` run's test selection covers fewer lanes than its clippy one.
pub(crate) fn feature_drop_lines(phases: &[(&[PlannedSweep], &PhaseSelection)]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut push = |line: String| {
        if !lines.contains(&line) {
            lines.push(line);
        }
    };
    for (sweeps, phase) in phases.iter().filter(|(_, p)| p.is_enabled()) {
        for (_, sweep, entry) in phase.entries().iter_with(sweeps) {
            // Attempted and deduped lanes alike: a dedupe decides who runs,
            // never whether a lane's narrowing is said.
            let drops = match entry {
                LaneEntry::Attempt(a) => a.drop_records(),
                LaneEntry::Deduped(d) => d.drops.clone(),
                _ => continue,
            };
            for (scope, d) in &drops {
                push(drop_line(&sweep.label, scope, d));
            }
        }
    }
    lines
}

impl Attempt {
    /// Every token this lane's requests dropped, with the request's scope
    /// as the announcement names it (`-p x`, `build y`).
    fn drop_records(&self) -> Vec<(String, DroppedToken)> {
        let mut out = Vec::new();
        for run in &self.runs {
            let scope = match run.selection.packages() {
                Some(p) => p
                    .iter()
                    .map(|p| format!("-p {p}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                None => "default selection".to_owned(),
            };
            out.extend(run.dropped().iter().map(|d| (scope.clone(), d.clone())));
        }
        for s in &self.support {
            out.extend(
                s.dropped()
                    .iter()
                    .map(|d| (format!("build {}", s.package()), d.clone())),
            );
        }
        out
    }
}

fn drop_line(label: &str, scope: &str, d: &DroppedToken) -> String {
    format!(
        "sweep {label}, {scope}: dropped {} (applies to {} only)",
        d.token,
        d.routed_by.join(", ")
    )
}

/// The member-check phase's selection. Which members exist is a cargo
/// metadata fact, so the member set itself is resolved when the phase runs;
/// this decides only whether it runs.
#[derive(Debug, Clone)]
pub(crate) enum MemberSelection {
    /// The phase does not apply to this run: no `[member_check]`, its
    /// cadence rules the claim out, or the phase is skipped.
    NotApplicable(&'static str),
    /// Run it, over the workspace members (less `skip`), intersected with
    /// the invocation's `-p` set when there is one.
    Eligible {
        cfg: crate::config::MemberCheckConfig,
        cli: Vec<String>,
    },
}

impl MemberSelection {
    pub(crate) fn build(
        cfg: Option<&crate::config::MemberCheckConfig>,
        cli: &[String],
        certifies: Option<Certifies>,
        enabled: bool,
    ) -> Self {
        if !enabled {
            return Self::NotApplicable(PHASE_SKIPPED);
        }
        let Some(cfg) = cfg else {
            return Self::NotApplicable("no [member_check] table or [bin] install list");
        };
        if !crate::config::Claim::new(certifies).runs(cfg.when) {
            return Self::NotApplicable("the member check's cadence rules this run out");
        }
        Self::Eligible {
            cfg: cfg.clone(),
            cli: cli.to_vec(),
        }
    }
}

/// The `[[fixture_lint]]` entries this run lints: each entry's own cadence
/// against the claim. Independent of `-p` - a fixture is not a workspace
/// package.
pub(crate) fn fixture_lint_selection(
    entries: &[crate::config::FixtureLint],
    certifies: Option<Certifies>,
    enabled: bool,
) -> Vec<crate::config::FixtureLint> {
    if !enabled {
        return Vec::new();
    }
    let claim = crate::config::Claim::new(certifies);
    entries
        .iter()
        .filter(|e| claim.runs(e.when))
        .cloned()
        .collect()
}

/// Every lane one `brokkr check` run plans: the tested sweeps (configured,
/// then any doctest carriers a complete claim derived), then the
/// `[[lint_sweep]]` lanes. Clippy and rustdoc see every lane; the test
/// machinery - preparation, the test phase, the coverage audit, the doc-only
/// rules - sees only the tested prefix. The lint lanes sit at the end, so a
/// [`LaneIdx`] means the same sweep in every phase that has it.
///
/// A lint lane equal to a tested build folds onto it in clippy and rustdoc
/// (never the reverse: dedupe folds onto an earlier lane), and label
/// reservation may rename a derived doctest carrier. The tested lanes'
/// compile inputs, test obligations and preparation scheduling are untouched.
#[derive(Debug, Clone)]
pub(crate) struct RunSweeps {
    lanes: Vec<PlannedSweep>,
    tested: usize,
}

impl RunSweeps {
    /// Refuses a lint label equal to a tested one: the two would read as one
    /// sweep in the `--json` trailer. Load validation and carrier label
    /// reservation leave only the legacy `all-features` sweep to reach this.
    pub(crate) fn new(
        tested: Vec<PlannedSweep>,
        lint: Vec<PlannedSweep>,
    ) -> Result<Self, DevError> {
        if let Some(clash) = lint
            .iter()
            .find(|l| tested.iter().any(|t| t.label == l.label))
        {
            return Err(DevError::Config(format!(
                "[[lint_sweep]] '{}' has the label of a sweep this run tests; rename the lint sweep",
                clash.label
            )));
        }
        let count = tested.len();
        let mut lanes = tested;
        lanes.extend(lint);
        Ok(Self {
            lanes,
            tested: count,
        })
    }

    /// A run with no lint lanes: every lane is tested.
    #[cfg(test)]
    pub(crate) fn tested_only(tested: Vec<PlannedSweep>) -> Self {
        let count = tested.len();
        Self {
            lanes: tested,
            tested: count,
        }
    }

    /// The sweeps the test machinery runs: the lanes before the lint lanes.
    pub(crate) fn tested(&self) -> &[PlannedSweep] {
        &self.lanes[..self.tested]
    }

    /// The lint lanes alone.
    pub(crate) fn lint(&self) -> &[PlannedSweep] {
        &self.lanes[self.tested..]
    }

    /// Every lane, tested first: what clippy and rustdoc select over.
    pub(crate) fn linted(&self) -> &[PlannedSweep] {
        &self.lanes
    }
}

/// Every selection one `brokkr check` run makes.
#[derive(Debug, Clone)]
pub(crate) struct CheckSelections {
    pub(crate) clippy: PhaseSelection,
    pub(crate) rustdoc: PhaseSelection,
    pub(crate) test: PhaseSelection,
    pub(crate) members: MemberSelection,
    pub(crate) fixtures: Vec<crate::config::FixtureLint>,
}

impl CheckSelections {
    /// `skip` is the run's phase-skip predicate (profile `skip_phases` and
    /// the prose-only shortcut); a skipped phase constructs nothing. Clippy
    /// and rustdoc select over every lane, the test phase over the tested
    /// ones ([`RunSweeps`]).
    pub(crate) fn build(
        sweeps: &RunSweeps,
        cli: &[String],
        skip: &dyn Fn(crate::check_cmd::Phase) -> bool,
        rustdoc_configured: bool,
        gated: GatedPhases<'_>,
        certifies: Option<Certifies>,
        oracle: &FeatureOracle<'_>,
    ) -> Result<Self, DevError> {
        let phase = |p: SelectionPhase, off: Option<&'static str>| {
            let lanes = match p {
                SelectionPhase::Test => sweeps.tested(),
                SelectionPhase::Clippy | SelectionPhase::Rustdoc => sweeps.linted(),
            };
            match off {
                Some(reason) => Ok(PhaseSelection::disabled(p, lanes, reason)),
                None => PhaseSelection::for_check(p, lanes, cli, oracle),
            }
        };
        let rustdoc_off = if skip(crate::check_cmd::Phase::Rustdoc) {
            Some(PHASE_SKIPPED)
        } else if !rustdoc_configured {
            Some("no [rustdoc] table")
        } else {
            None
        };
        Ok(Self {
            clippy: phase(
                SelectionPhase::Clippy,
                skip(crate::check_cmd::Phase::Clippy).then_some(PHASE_SKIPPED),
            )?,
            rustdoc: phase(SelectionPhase::Rustdoc, rustdoc_off)?,
            test: phase(
                SelectionPhase::Test,
                skip(crate::check_cmd::Phase::Test).then_some(PHASE_SKIPPED),
            )?,
            members: MemberSelection::build(
                gated.member_check,
                cli,
                certifies,
                !skip(crate::check_cmd::Phase::MemberCheck),
            ),
            fixtures: fixture_lint_selection(
                gated.fixture_lints,
                certifies,
                !skip(crate::check_cmd::Phase::FixtureLint),
            ),
        })
    }
}

/// The config of the gate-priced compiling phases a check run may select.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GatedPhases<'a> {
    pub(crate) member_check: Option<&'a crate::config::MemberCheckConfig>,
    pub(crate) fixture_lints: &'a [crate::config::FixtureLint],
}

/// Which sweeps a run actually worked on, for the `--json` trailer's
/// `sweeps` list: one flag per sweep for the diagnostics group (clippy and
/// rustdoc share it) and one for the test phase, each set when the phase
/// starts working on that sweep - before cargo returns, so a lane that
/// failed still counts as reached. The trailer lists their union: a sweep
/// may be reached by one group, the other, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReachLedger {
    lanes: Lanes<ReachState>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ReachState {
    diagnostics: bool,
    test: bool,
}

impl ReachLedger {
    pub(crate) fn new(sweeps: &[PlannedSweep]) -> Self {
        Self {
            lanes: Lanes::from_sweeps(sweeps, |_, _| ReachState::default()),
        }
    }

    pub(crate) fn reach_diagnostics(&mut self, sweep: LaneIdx) {
        self.lanes
            .get_mut(sweep)
            .expect("reach index comes from the active sweep list")
            .diagnostics = true;
    }

    pub(crate) fn reach_test(&mut self, sweep: LaneIdx) {
        self.lanes
            .get_mut(sweep)
            .expect("reach index comes from the active sweep list")
            .test = true;
    }

    /// The labels of the sweeps either group reached, in sweep order.
    pub(crate) fn reached_labels<'a>(&self, sweeps: &'a [PlannedSweep]) -> Vec<&'a str> {
        self.lanes
            .iter_with(sweeps)
            .filter(|(_, _, reached)| reached.diagnostics || reached.test)
            .map(|(_, sweep, _)| sweep.label.as_str())
            .collect()
    }
}

pub(crate) fn join_notes(notes: &[AdmissionNote]) -> String {
    notes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn unique(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(list.len());
    for item in list {
        if !out.contains(item) {
            out.push(item.clone());
        }
    }
    out
}

fn has_duplicates(list: &[String]) -> bool {
    list.iter().enumerate().any(|(i, a)| list[..i].contains(a))
}

impl Selection {
    /// The sweep's own selection for `phase`, with no invocation override.
    /// Parse refuses a sweep setting both `packages` and
    /// `test_exclude_packages`, so the order here never decides anything.
    pub(crate) fn configured(sweep: &PlannedSweep, phase: SelectionPhase) -> Self {
        if !sweep.packages.is_empty() {
            Self::Explicit(unique(&sweep.packages))
        } else if phase.honours_test_exclusions() && !sweep.test_exclude_packages.is_empty() {
            Self::WorkspaceExcluding(unique(&sweep.test_exclude_packages))
        } else {
            Self::Bare
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::config::{FeatureUnification, SweepProfile};

    fn planned(declaration: crate::profile::SweepDecl) -> PlannedSweep {
        PlannedSweep::resolve(declaration, false)
    }

    fn with_sweep(
        sweep: PlannedSweep,
        update: impl FnOnce(&mut crate::profile::SweepDecl),
    ) -> PlannedSweep {
        let mut declaration = sweep.into_declaration();
        update(&mut declaration);
        planned(declaration)
    }

    fn s(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| (*p).to_owned()).collect()
    }

    fn sweep(label: &str) -> PlannedSweep {
        planned(crate::profile::SweepDecl {
            label: label.into(),
            ..Default::default()
        })
    }

    #[test]
    fn a_target_is_build_shape_and_leaves_host_shapes_unchanged() {
        let host = sweep("host");
        let musl = with_sweep(sweep("musl"), |d| {
            d.target = Some("x86_64-unknown-linux-musl".into());
        });
        let host_shape = DeclaredShape::of(&host);
        let musl_shape = DeclaredShape::of(&musl);
        assert_ne!(host_shape.id(), musl_shape.id());
        // A host shape encodes exactly what it did before the key existed:
        // nothing is appended for an unset target.
        let mut expected = StableEncoder::default();
        expected.string("brokkr.declared-shape.v1");
        expected.strings(&[]);
        expected.strings(&[]);
        expected.strings(&host_shape.features);
        expected.strings(&[]);
        expected.pairs(&[]);
        expected.profile(None);
        expected.unification(host.effective_unification());
        assert_eq!(host_shape.stable_encoding(), expected.finish());
        // Every cargo run of a target sweep names the triple.
        assert_eq!(
            crate::build_purpose::triple_args(&musl),
            s(&["--target", "x86_64-unknown-linux-musl"])
        );
        assert!(crate::build_purpose::triple_args(&host).is_empty());
    }

    fn listed(label: &str, packages: &[&str]) -> PlannedSweep {
        with_sweep(sweep(label), |sweep| sweep.packages = s(packages))
    }

    fn excluding(label: &str, excluded: &[&str]) -> PlannedSweep {
        with_sweep(sweep(label), |sweep| {
            sweep.test_exclude_packages = s(excluded);
        })
    }

    fn package_mode(sweep: PlannedSweep) -> PlannedSweep {
        with_sweep(sweep, |sweep| {
            sweep.feature_unification = FeatureUnification::Package;
        })
    }

    fn cli(packages: &[&str]) -> Selection {
        Selection::Override {
            packages: s(packages),
            provenance: Provenance::Cli,
        }
    }

    /// `for_check` over sweeps whose features never need metadata.
    fn for_check(
        phase: SelectionPhase,
        sweeps: &[PlannedSweep],
        cli: &[String],
    ) -> Result<PhaseSelection, DevError> {
        PhaseSelection::for_check(phase, sweeps, cli, &FeatureOracle::unavailable())
    }

    fn for_brokkr_test(
        sweeps: &[PlannedSweep],
        package: &str,
        source: PackageSource,
        debug_of: &dyn Fn(&PlannedSweep) -> bool,
    ) -> Result<PhaseSelection, DevError> {
        PhaseSelection::for_brokkr_test(
            sweeps,
            package,
            source,
            &[],
            debug_of,
            &FeatureOracle::unavailable(),
        )
    }

    #[test]
    fn the_exclusion_rule_is_checked_before_the_packages_list() {
        // A package both rules rule out is reported as excluded.
        let both = with_sweep(sweep("t"), |sweep| {
            sweep.packages = s(&["a"]);
            sweep.test_exclude_packages = s(&["x"]);
        });
        let a = admit(&both, &s(&["x"]), SelectionPhase::Test);
        assert_eq!(
            a.notes,
            vec![AdmissionNote {
                package: "x".into(),
                rule: AdmissionRule::TestExcluded
            }]
        );
        // Outside the test phase the exclusion list does not exist.
        let a = admit(&both, &s(&["x"]), SelectionPhase::Clippy);
        assert_eq!(a.notes[0].rule, AdmissionRule::NotListed);
        assert_eq!(
            a.notes[0].to_string(),
            "-p x is not in this sweep's packages list"
        );
    }

    #[test]
    fn admission_keeps_invocation_order_each_package_once() {
        let a = admit(&sweep("w"), &s(&["b", "a", "b"]), SelectionPhase::Test);
        assert_eq!(a.kept, s(&["b", "a"]));
        assert!(a.notes.is_empty());
    }

    #[test]
    fn test_exclusions_narrow_the_test_phase_only() {
        let sweeps = vec![excluding("default", &["x"])];
        let clippy = for_check(SelectionPhase::Clippy, &sweeps, &[]).unwrap();
        assert_eq!(
            clippy.attempt(LaneIdx::new(0)).unwrap().selection(),
            &Selection::Bare
        );
        let test = for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
        assert_eq!(
            test.attempt(LaneIdx::new(0)).unwrap().selection(),
            &Selection::WorkspaceExcluding(s(&["x"]))
        );
        assert_eq!(
            package_args(test.attempt(LaneIdx::new(0)).unwrap().selection()),
            s(&["--workspace", "--exclude", "x"])
        );

        // Under `-p x` the exclusion rules the lane out of the test phase
        // and nowhere else.
        let x = s(&["x"]);
        let clippy = for_check(SelectionPhase::Clippy, &sweeps, &x).unwrap();
        assert_eq!(
            clippy.attempt(LaneIdx::new(0)).unwrap().selection(),
            &cli(&["x"])
        );
        let test = for_check(SelectionPhase::Test, &sweeps, &x).unwrap();
        assert!(matches!(
            test.entry(LaneIdx::new(0)),
            Some(LaneEntry::Excluded(_))
        ));
        assert_eq!(
            test.entry(LaneIdx::new(0)).unwrap().skip_reason().unwrap(),
            "-p x is in this sweep's test_exclude_packages"
        );
    }

    #[test]
    fn an_override_replaces_the_sweeps_own_selection_with_what_its_rules_kept() {
        let sweeps = vec![listed("ffi", &["a", "b"])];
        let sel = for_check(SelectionPhase::Test, &sweeps, &s(&["a", "x"])).unwrap();
        let attempt = sel.attempt(LaneIdx::new(0)).unwrap();
        assert_eq!(attempt.selection(), &cli(&["a"]));
        assert_eq!(attempt.notes().len(), 1);
        assert_eq!(package_args(attempt.selection()), s(&["-p", "a"]));
        // Without an override the sweep's own list stands.
        let own = for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
        assert_eq!(
            own.attempt(LaneIdx::new(0)).unwrap().selection(),
            &Selection::Explicit(s(&["a", "b"]))
        );
    }

    #[test]
    fn package_mode_over_a_selection_naming_no_packages_is_an_error() {
        let bare = package_mode(sweep("pkg"));
        let err = for_check(SelectionPhase::Clippy, &[bare], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("names no packages"), "{err}");
        let excl = package_mode(excluding("pkg", &["x"]));
        assert!(for_check(SelectionPhase::Test, &[excl], &[]).is_err());
    }

    #[test]
    fn per_package_resolutions_equal_the_selection() {
        let sweeps = vec![package_mode(listed("pkg", &["a", "b", "c"]))];
        let own = for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
        let attempt = own.attempt(LaneIdx::new(0)).unwrap();
        assert_eq!(
            attempt.resolutions(),
            &Resolutions::PerPackage(s(&["a", "b", "c"]))
        );
        let runs = attempt.runs();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[1].resolution.as_deref(), Some("b"));
        assert_eq!(runs[1].selection, Selection::Explicit(s(&["b"])));

        // Narrowed by `-p`, in invocation order, each once.
        let narrowed = for_check(SelectionPhase::Test, &sweeps, &s(&["c", "a", "c"])).unwrap();
        let attempt = narrowed.attempt(LaneIdx::new(0)).unwrap();
        assert_eq!(
            attempt.resolutions(),
            &Resolutions::PerPackage(s(&["c", "a"]))
        );
        assert_eq!(
            attempt.selection().packages().unwrap(),
            s(&["c", "a"]).as_slice()
        );
        assert_eq!(attempt.runs()[0].selection, cli(&["c"]));

        // Any other mode is one combined resolution over the selection.
        let combined = for_check(SelectionPhase::Test, &[listed("x", &["a", "b"])], &[]).unwrap();
        let runs = combined.attempt(LaneIdx::new(0)).unwrap().runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].resolution, None);
        assert_eq!(runs[0].selection, Selection::Explicit(s(&["a", "b"])));
    }

    #[test]
    fn diagnostics_dedupe_only_onto_an_earlier_attempt() {
        // Two lanes of one entry: one build shape.
        let sweeps = vec![
            listed("tier1/x", &["a"]),
            listed("tier2/x", &["a"]),
            listed("other", &["b"]),
        ];
        let sel = for_check(SelectionPhase::Clippy, &sweeps, &[]).unwrap();
        assert!(sel.attempt(LaneIdx::new(0)).is_some());
        match sel.entry(LaneIdx::new(1)).unwrap() {
            LaneEntry::Deduped(d) => {
                assert_eq!(d.onto(), LaneIdx::new(0));
                assert!(sel.attempt(d.onto()).is_some(), "onto names an attempt");
            }
            other => panic!("expected deduped, got {other:?}"),
        }
        assert!(sel.attempt(LaneIdx::new(2)).is_some());

        // A lane the rules exclude is never a dedupe target: the first
        // lane of the shape that is ATTEMPTED is.
        let sweeps = vec![listed("a-only", &["a"]), listed("a-only-twin", &["a"])];
        let sel = for_check(SelectionPhase::Clippy, &sweeps, &s(&["z"])).unwrap();
        assert!(
            sel.entries()
                .iter()
                .all(|e| matches!(e, LaneEntry::Excluded(_)))
        );

        // The test phase never dedupes.
        let sweeps = vec![listed("tier1/x", &["a"]), listed("tier2/x", &["a"])];
        let test = for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
        assert!(test.entries().iter().all(|e| e.attempt().is_some()));
    }

    #[test]
    fn rustdoc_does_not_apply_to_doctest_carriers() {
        let doc = with_sweep(excluding("docs", &["bin"]), |sweep| {
            sweep.lane_exec = crate::profile::LaneExec::doc_only();
        });
        let sweeps = vec![sweep("default"), doc];
        let sel = for_check(SelectionPhase::Rustdoc, &sweeps, &s(&["lib"])).unwrap();
        assert!(sel.attempt(LaneIdx::new(0)).is_some());
        match sel.entry(LaneIdx::new(1)).unwrap() {
            LaneEntry::NotApplicable(n) => assert_eq!(n.admission().kept, s(&["lib"])),
            other => panic!("expected not applicable, got {other:?}"),
        }
        // A rustdoc phase over carriers alone is not refused.
        let only_doc = vec![with_sweep(sweep("docs"), |sweep| {
            sweep.lane_exec = crate::profile::LaneExec::doc_only();
        })];
        let sel = for_check(SelectionPhase::Rustdoc, &only_doc, &s(&["x"])).unwrap();
        assert!(sel.refusal().is_none());
    }

    fn with_exec(sweep: PlannedSweep, exec: LaneExec) -> PlannedSweep {
        with_sweep(sweep, |d| d.lane_exec = exec)
    }

    fn derive(sweeps: &[PlannedSweep], facts: &crate::check_cmd::DocFacts) -> Vec<PlannedSweep> {
        derive_doctest_carriers(sweeps, &[], facts, &FeatureOracle::unavailable()).unwrap()
    }

    /// A lint lane's label is reserved: the carrier takes the next free one
    /// rather than reading as the lint lane in the run's sweep list.
    #[test]
    fn a_carrier_does_not_take_a_lint_lane_label() {
        let facts = crate::check_cmd::DocFacts::for_members(&[("a", true)]);
        let a = listed("a", &["a"]);
        let labels = |reserved: &[&str]| -> Vec<String> {
            derive_doctest_carriers(
                std::slice::from_ref(&a),
                reserved,
                &facts,
                &FeatureOracle::unavailable(),
            )
            .unwrap()
            .into_iter()
            .map(|c| c.label.clone())
            .collect()
        };
        assert_eq!(labels(&[]), ["a (doctests)"]);
        assert_eq!(labels(&["a (doctests)"]), ["a (doctests 2)"]);
    }

    /// brokkr#22: a complete claim owes every tested build's doctests, not
    /// one carrier somewhere. A parallel lane gets a carrier of exactly its
    /// build - filters dropped, run under the watchdog, its unification
    /// answer kept; its nextest twin (same build) adds none; a `doc_only`
    /// entry of a lane's build discharges it; a lane reaching no doctested
    /// member, and a curated one, owe nothing.
    #[test]
    fn a_complete_claim_derives_one_carrier_per_tested_build_without_one() {
        let facts =
            crate::check_cmd::DocFacts::for_members(&[("a", true), ("b", true), ("bin", false)]);
        let a = with_sweep(
            with_exec(listed("a", &["a"]), LaneExec::Parallel { budget: 4 }),
            |d| {
                d.filters.include_ignored = true;
            },
        );
        let a_twin = with_exec(
            listed("a-serial", &["a"]),
            LaneExec::Nextest { threads: None },
        );
        let b = with_exec(listed("b", &["b"]), LaneExec::Parallel { budget: 4 });
        let b_doc = with_exec(listed("b-doc", &["b"]), LaneExec::doc_only());
        let bin = listed("bin", &["bin"]);
        let curated = with_sweep(listed("cur", &["a"]), |d| {
            d.curated = true;
            d.env.insert("SHAPE".into(), "curated".into());
        });
        let sweeps = vec![a.clone(), a_twin, b, b_doc, bin, curated];
        let derived = derive(&sweeps, &facts);
        assert_eq!(
            derived.len(),
            1,
            "{:?}",
            derived.iter().map(|s| &s.label).collect::<Vec<_>>()
        );
        let carrier = &derived[0];
        assert_eq!(carrier.label, "a (doctests)");
        assert_eq!(
            carrier.origin,
            crate::profile::SweepOrigin::DoctestCarrier { source: "a".into() }
        );
        assert!(carrier.lane_exec().is_doc_only());
        assert!(
            !carrier
                .lane_exec()
                .threads()
                .is_some_and(crate::profile::Threads::is_cargo_mediated)
        );
        assert_eq!(
            carrier.lane_filters(),
            &crate::profile::LaneFilters::default()
        );
        assert_eq!(carrier.packages, a.packages);
        assert_eq!(carrier.effective_unification(), a.effective_unification());

        // The carrier is a test lane only: clippy and rustdoc lint its source.
        let mut all = sweeps;
        all.extend(derived);
        let last = LaneIdx::new(all.len() - 1);
        for phase in [SelectionPhase::Clippy, SelectionPhase::Rustdoc] {
            let sel = for_check(phase, &all, &[]).unwrap();
            assert!(
                matches!(sel.entry(last), Some(LaneEntry::NotApplicable(_))),
                "{phase:?}"
            );
        }
        assert!(
            for_check(SelectionPhase::Test, &all, &[])
                .unwrap()
                .attempt(last)
                .is_some()
        );

        // Once derived, a build is carried: a second derivation adds nothing.
        assert!(derive(&all, &facts).is_empty());
    }

    /// The derived label never collides with a configured one - preparation
    /// hands builds over by label - and identity is the typed origin either way.
    #[test]
    fn a_derived_carrier_label_never_collides() {
        let facts = crate::check_cmd::DocFacts::for_members(&[("a", true)]);
        let sweeps = vec![
            with_exec(listed("a", &["a"]), LaneExec::Parallel { budget: 2 }),
            with_sweep(listed("a (doctests)", &["a"]), |d| {
                d.env.insert("X".into(), "1".into());
            }),
        ];
        let labels: Vec<String> = derive(&sweeps, &facts)
            .iter()
            .map(|s| s.label.clone())
            .collect();
        assert_eq!(labels, s(&["a (doctests 2)", "a (doctests) (doctests)"]));
    }

    /// A package-mode lane's carrier keeps its per-package resolutions, and
    /// owes a stream only on the ones that reach a doctested member: a
    /// bin-only package beside a library neither blocks the carrier nor is
    /// required to present doctests.
    #[test]
    fn a_package_mode_carrier_owes_doctests_per_resolution() {
        let facts = crate::check_cmd::DocFacts::for_members(&[("a", true), ("bin", false)]);
        let lane = package_mode(with_exec(
            listed("pkg", &["a", "bin"]),
            LaneExec::Parallel { budget: 2 },
        ));
        let derived = derive(std::slice::from_ref(&lane), &facts);
        assert_eq!(derived.len(), 1);
        let sel = for_check(SelectionPhase::Test, &derived, &[]).unwrap();
        let attempt = sel.attempt(LaneIdx::new(0)).unwrap();
        assert_eq!(
            attempt.runs().len(),
            2,
            "one resolution per package, as the source lane"
        );
        assert_eq!(
            facts.obliged_resolutions(attempt),
            vec![Resolution::Package(PkgName("a".into()))]
        );
        // A package-mode lane over bin-only packages owes nothing.
        let bins = package_mode(listed("bins", &["bin"]));
        assert!(derive(&[bins], &facts).is_empty());
    }

    #[test]
    fn a_disabled_phase_constructs_and_validates_nothing() {
        // Package mode with no packages would be a construction error -
        // but not in a phase that does not run.
        let invalid = package_mode(sweep("pkg"));
        let sweeps = [invalid.clone(), sweep("other")];
        let sel = PhaseSelection::disabled(SelectionPhase::Test, &sweeps, "skipped");
        assert_eq!(sel.entries().len(), 2);
        assert!(
            sel.entries()
                .iter()
                .all(|e| matches!(e, LaneEntry::Disabled(_)) && e.notes().is_empty())
        );
        assert!(!sel.is_enabled() && sel.refusal().is_none());

        use crate::check_cmd::Phase;
        let skip = |p: Phase| matches!(p, Phase::Test | Phase::Clippy);
        let oracle = FeatureOracle::unavailable();
        let all = CheckSelections::build(
            &RunSweeps::tested_only(vec![invalid.clone()]),
            &[],
            &skip,
            false,
            GatedPhases::default(),
            None,
            &oracle,
        )
        .unwrap();
        assert!(!all.clippy.is_enabled() && !all.test.is_enabled() && !all.rustdoc.is_enabled());
        // The same sweep in an enabled phase is the construction error.
        assert!(
            CheckSelections::build(
                &RunSweeps::tested_only(vec![invalid]),
                &[],
                &|_| false,
                false,
                GatedPhases::default(),
                None,
                &oracle
            )
            .is_err()
        );
    }

    /// Lint lanes reach clippy and rustdoc only, after the tested lanes, and
    /// one compiling a tested lane's build folds onto it there.
    #[test]
    fn lint_lanes_are_selected_by_the_diagnostic_phases_alone() {
        let lanes = RunSweeps::new(
            vec![sweep("default")],
            vec![sweep("same-shape"), listed("arm", &["a"])],
        )
        .unwrap();
        assert_eq!(lanes.tested().len(), 1);
        assert_eq!(lanes.lint().len(), 2);
        let oracle = FeatureOracle::unavailable();
        let all = CheckSelections::build(
            &lanes,
            &[],
            &|_| false,
            true,
            GatedPhases::default(),
            None,
            &oracle,
        )
        .unwrap();
        assert_eq!(all.test.entries().len(), 1);
        for phase in [&all.clippy, &all.rustdoc] {
            assert_eq!(phase.entries().len(), 3);
            assert!(phase.attempt(LaneIdx::new(0)).is_some());
            assert!(
                matches!(phase.entry(LaneIdx::new(1)), Some(LaneEntry::Deduped(d)) if d.onto() == LaneIdx::new(0))
            );
            assert!(phase.attempt(LaneIdx::new(2)).is_some());
        }
        // Each selection is read against the lanes it was built over.
        assert!(
            feature_drop_lines(&[(lanes.linted(), &all.clippy), (lanes.tested(), &all.test),])
                .is_empty()
        );
        // A lint label equal to a tested one would be one name for two sweeps.
        let err =
            RunSweeps::new(vec![sweep("all-features")], vec![sweep("all-features")]).unwrap_err();
        assert!(err.to_string().contains("'all-features'"), "{err}");
    }

    #[test]
    fn a_refusal_is_stored_not_raised() {
        let sweeps = vec![listed("ffi", &["a"]), listed("vm", &["b"])];
        let test = for_check(SelectionPhase::Test, &sweeps, &s(&["z"])).unwrap();
        assert_eq!(
            test.refusal().unwrap().to_string(),
            "-p z: every sweep's config rules the selection out; zero tests ran"
        );
        assert!(test.check_refusal().is_err());
        let clippy = for_check(SelectionPhase::Clippy, &sweeps, &s(&["z"])).unwrap();
        assert_eq!(
            clippy.refusal().unwrap().to_string(),
            "-p z: every sweep's config rules the selection out (ffi, vm); nothing reached clippy"
        );
        // One admitting sweep is enough; no `-p` never refuses.
        let ok = for_check(SelectionPhase::Test, &sweeps, &s(&["a"])).unwrap();
        assert!(ok.refusal().is_none());
        assert!(
            for_check(SelectionPhase::Test, &sweeps, &[])
                .unwrap()
                .refusal()
                .is_none()
        );
    }

    fn debug_of(debug: bool) -> impl Fn(&PlannedSweep) -> bool {
        move |_| debug
    }

    #[test]
    fn exec_projection_collapses_profile_pins_under_one_override() {
        // `--debug` overrides both sweeps' pins: they execute identically.
        let dev = with_sweep(sweep("dev"), |sweep| {
            sweep.profile = Some(SweepProfile::Dev);
        });
        let rel = with_sweep(sweep("rel"), |sweep| {
            sweep.profile = Some(SweepProfile::Release);
        });
        let sel = for_brokkr_test(
            &[dev.clone(), rel.clone()],
            "a",
            PackageSource::Cli,
            &debug_of(true),
        )
        .unwrap();
        assert!(
            matches!(sel.entry(LaneIdx::new(1)), Some(LaneEntry::Deduped(d)) if d.onto() == LaneIdx::new(0))
        );
        // Without the override each pin decides its own debug answer.
        let pinned = |s: &PlannedSweep| s.profile == Some(SweepProfile::Dev);
        let sel = for_brokkr_test(&[dev, rel], "a", PackageSource::Cli, &pinned).unwrap();
        assert!(sel.entries().iter().all(|e| e.attempt().is_some()));
    }

    #[test]
    fn exec_projection_collapses_two_sweeps_narrowed_to_one_package() {
        let sweeps = vec![listed("ab", &["a", "b"]), listed("ac", &["a", "c"])];
        let sel = for_brokkr_test(
            &sweeps,
            "a",
            PackageSource::DefaultPackage,
            &debug_of(false),
        )
        .unwrap();
        assert_eq!(
            sel.attempt(LaneIdx::new(0)).unwrap().selection(),
            &Selection::Override {
                packages: s(&["a"]),
                provenance: Provenance::ResolvedPackage(PackageSource::DefaultPackage)
            }
        );
        assert!(matches!(
            sel.entry(LaneIdx::new(1)),
            Some(LaneEntry::Deduped(_))
        ));
    }

    #[test]
    fn exec_projection_keeps_a_doctest_twin_and_differing_support_builds_apart() {
        let twin = with_sweep(sweep("docs"), |sweep| {
            sweep.lane_exec = crate::profile::LaneExec::doc_only();
        });
        let sel = for_brokkr_test(
            &[sweep("default"), twin],
            "a",
            PackageSource::Cli,
            &debug_of(false),
        )
        .unwrap();
        assert!(
            sel.entries().iter().all(|e| e.attempt().is_some()),
            "the doc twin is its own run"
        );

        let server = with_sweep(sweep("server"), |sweep| {
            sweep.build_packages = s(&["server"]);
        });
        let sel = for_brokkr_test(
            &[sweep("plain"), server],
            "a",
            PackageSource::Cli,
            &debug_of(false),
        )
        .unwrap();
        assert!(sel.entries().iter().all(|e| e.attempt().is_some()));
    }

    #[test]
    fn exec_projection_ignores_provenance() {
        let sw = sweep("x");
        let oracle = FeatureOracle::unavailable();
        let of = |selection: Selection| {
            let a = Attempt::build(
                &sw,
                selection,
                Resolutions::Combined,
                Vec::new(),
                &Selection::Bare,
                &oracle,
            )
            .unwrap();
            ExecProjection::of(&sw, &a, false)
        };
        let from_cli = of(cli(&["a"]));
        let from_default = of(Selection::Override {
            packages: s(&["a"]),
            provenance: Provenance::ResolvedPackage(PackageSource::ProjectDefault),
        });
        let explicit = of(Selection::Explicit(s(&["a"])));
        assert_eq!(from_cli, from_default);
        assert_eq!(from_cli, explicit);
    }

    // ---- feature projection ------------------------------------------

    use crate::build_purpose::features::fixture;

    /// A sweep over `packages` with configured feature `tokens`.
    fn featured(label: &str, packages: &[&str], tokens: &[&str]) -> PlannedSweep {
        with_sweep(listed(label, packages), |sweep| {
            sweep.features = crate::profile::FeatureConfig {
                tokens: s(tokens),
                ..Default::default()
            };
        })
    }

    const PINERS: [&str; 4] = [
        "piners-vm",
        "piners-strategy",
        "piners-runner",
        "piners-harness",
    ];
    const PINERS_TOKENS: [&str; 4] = [
        "piners-vm/opcode-counts",
        "piners-strategy/hotpath",
        "piners-runner/trace",
        "piners-harness/bench",
    ];

    fn piners_sweep() -> PlannedSweep {
        featured("piners", &PINERS, &PINERS_TOKENS)
    }

    #[test]
    fn a_full_run_carries_every_token_without_reading_metadata() {
        // The configured selection is the domain: nothing can drop, and
        // the oracle (which would fail) is never consulted.
        let sel = for_check(SelectionPhase::Clippy, &[piners_sweep()], &[]).unwrap();
        let run = &sel.attempt(LaneIdx::new(0)).unwrap().runs()[0];
        assert_eq!(
            run.feature_args(),
            s(&["--features", &PINERS_TOKENS.join(",")]).as_slice()
        );
        assert!(run.dropped().is_empty());
    }

    #[test]
    fn dash_p_keeps_only_the_tokens_the_narrowed_selection_routes() {
        let oracle = FeatureOracle::fixed(fixture::piners());
        let sweeps = [piners_sweep()];
        // piners-strategy defines `hotpath` and depends on piners-vm, so
        // `piners-vm/opcode-counts` routes too; runner and harness do not.
        let sel = PhaseSelection::for_check(
            SelectionPhase::Test,
            &sweeps,
            &s(&["piners-strategy"]),
            &oracle,
        )
        .unwrap();
        let run = &sel.attempt(LaneIdx::new(0)).unwrap().runs()[0];
        assert_eq!(
            run.cargo_args(),
            s(&[
                "-p",
                "piners-strategy",
                "--features",
                "piners-vm/opcode-counts,piners-strategy/hotpath"
            ])
        );
        let dropped: Vec<&str> = run.dropped().iter().map(|d| d.token.as_str()).collect();
        assert_eq!(dropped, ["piners-runner/trace", "piners-harness/bench"]);
        assert_eq!(
            feature_drop_lines(&[(&sweeps[..], &sel)]),
            s(&[
                "sweep piners, -p piners-strategy: dropped piners-runner/trace (applies to piners-runner, piners-harness only)",
                "sweep piners, -p piners-strategy: dropped piners-harness/bench (applies to piners-harness only)",
            ])
        );

        // piners-harness depends on every other member: all kept.
        let sel = PhaseSelection::for_check(
            SelectionPhase::Test,
            &sweeps,
            &s(&["piners-harness"]),
            &oracle,
        )
        .unwrap();
        let run = &sel.attempt(LaneIdx::new(0)).unwrap().runs()[0];
        assert_eq!(
            run.feature_args(),
            s(&["--features", &PINERS_TOKENS.join(",")]).as_slice()
        );
        assert!(run.dropped().is_empty());
    }

    #[test]
    fn brokkr_test_projects_onto_its_one_package() {
        let oracle = FeatureOracle::fixed(fixture::piners());
        let sel = PhaseSelection::for_brokkr_test(
            &[piners_sweep()],
            "piners-vm",
            PackageSource::Cli,
            &[],
            &debug_of(false),
            &oracle,
        )
        .unwrap();
        let run = &sel.attempt(LaneIdx::new(0)).unwrap().runs()[0];
        assert_eq!(
            run.feature_args(),
            s(&["--features", "piners-vm/opcode-counts"]).as_slice()
        );
        assert_eq!(run.dropped().len(), 3);
    }

    #[test]
    fn package_mode_projects_each_resolution_without_dash_p() {
        let oracle = FeatureOracle::fixed(fixture::piners());
        let sweeps = [package_mode(piners_sweep())];
        let sel = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &[], &oracle).unwrap();
        let attempt = sel.attempt(LaneIdx::new(0)).unwrap();
        let features: Vec<&[String]> = attempt
            .runs()
            .iter()
            .map(ResolutionRun::feature_args)
            .collect();
        assert_eq!(
            features,
            [
                s(&["--features", "piners-vm/opcode-counts"]).as_slice(),
                s(&[
                    "--features",
                    "piners-vm/opcode-counts,piners-strategy/hotpath"
                ])
                .as_slice(),
                s(&["--features", "piners-vm/opcode-counts,piners-runner/trace"]).as_slice(),
                s(&["--features", &PINERS_TOKENS.join(",")]).as_slice(),
            ]
        );
        // The lane-level description carries every token some run carries.
        assert_eq!(
            attempt.described_features(),
            s(&["--features", &PINERS_TOKENS.join(",")]).as_slice()
        );
    }

    #[test]
    fn a_token_no_domain_member_routes_is_left_for_cargo() {
        let oracle = FeatureOracle::fixed(fixture::piners());
        let sweeps = [featured(
            "typo",
            &PINERS,
            &["piners-vm/opcode-count", "nonsense", "a/b/c"],
        )];
        let sel = PhaseSelection::for_check(
            SelectionPhase::Clippy,
            &sweeps,
            &s(&["piners-runner"]),
            &oracle,
        )
        .unwrap();
        let run = &sel.attempt(LaneIdx::new(0)).unwrap().runs()[0];
        // `piners-vm/...` routes through runner's dependency on vm (the
        // feature name is cargo's to check); the rest route nowhere.
        assert_eq!(
            run.feature_args(),
            s(&["--features", "piners-vm/opcode-count,nonsense,a/b/c"]).as_slice()
        );
        assert!(run.dropped().is_empty());
    }

    #[test]
    fn support_builds_project_onto_the_support_package() {
        let oracle = FeatureOracle::fixed(fixture::piners());
        // The lane tests vm and strategy; the support build is the runner.
        let sweep = with_sweep(
            featured(
                "bin",
                &["piners-vm", "piners-strategy"],
                &[
                    "piners-vm/opcode-counts",
                    "piners-strategy/hotpath",
                    "piners-runner/trace",
                ],
            ),
            |sweep| sweep.build_packages = s(&["piners-runner"]),
        );
        let sel = PhaseSelection::for_check(
            SelectionPhase::Test,
            std::slice::from_ref(&sweep),
            &[],
            &oracle,
        )
        .unwrap();
        let attempt = sel.attempt(LaneIdx::new(0)).unwrap();
        let support = &attempt.support()[0];
        assert_eq!(support.package(), "piners-runner");
        assert_eq!(
            support.feature_args(),
            s(&["--features", "piners-vm/opcode-counts,piners-runner/trace"]).as_slice()
        );
        assert_eq!(support.package_args(), s(&["-p", "piners-runner"]));
        assert_eq!(support.dropped()[0].token, "piners-strategy/hotpath");
        // The lane's own run IS its configured selection, so it is the
        // identity - `piners-runner/trace` included, which no lane package
        // routes: cargo rejects it there exactly as a full run always has.
        // Only support builds widen the domain by `build_packages`.
        let run = &attempt.runs()[0];
        assert!(
            run.dropped().is_empty(),
            "the test run's selection is the domain"
        );
        assert!(run.feature_args()[1].contains("piners-runner/trace"));
        let lines = feature_drop_lines(&[(std::slice::from_ref(&sweep), &sel)]);
        assert_eq!(
            lines,
            s(&[
                "sweep bin, build piners-runner: dropped piners-strategy/hotpath (applies to piners-strategy only)"
            ])
        );
    }

    #[test]
    fn invocation_explicit_features_are_never_projected() {
        let adhoc = with_sweep(piners_sweep(), |sweep| {
            sweep.features.invocation_explicit = true;
        });
        // No oracle consulted, every token kept, under `-p` too.
        let sel = for_check(SelectionPhase::Test, &[adhoc], &s(&["piners-vm"])).unwrap();
        let run = &sel.attempt(LaneIdx::new(0)).unwrap().runs()[0];
        assert_eq!(
            run.feature_args(),
            s(&["--features", &PINERS_TOKENS.join(",")]).as_slice()
        );
    }

    #[test]
    fn diagnostics_dedupe_on_the_effective_key() {
        let oracle = FeatureOracle::fixed(fixture::piners());
        // Two sweeps differing only in a token piners-vm does not route:
        // under `-p piners-vm` they compile the same thing.
        let a = featured("a", &PINERS, &["piners-vm/opcode-counts"]);
        let b = featured(
            "b",
            &PINERS,
            &["piners-vm/opcode-counts", "piners-runner/trace"],
        );
        let sweeps = [a.clone(), b.clone()];
        let narrowed =
            PhaseSelection::for_check(SelectionPhase::Clippy, &sweeps, &s(&["piners-vm"]), &oracle)
                .unwrap();
        assert!(
            matches!(narrowed.entry(LaneIdx::new(1)), Some(LaneEntry::Deduped(d)) if d.onto() == LaneIdx::new(0))
        );
        // Without `-p` their requests differ: both attempted.
        let full =
            PhaseSelection::for_check(SelectionPhase::Clippy, &sweeps, &[], &oracle).unwrap();
        assert!(full.entries().iter().all(|e| e.attempt().is_some()));
        // A compile input outside the requests still keeps them apart.
        let c = with_sweep(a, |sweep| sweep.rustflags = s(&["--cfg", "x"]));
        let apart =
            PhaseSelection::for_check(SelectionPhase::Clippy, &[b, c], &s(&["piners-vm"]), &oracle)
                .unwrap();
        assert!(apart.entries().iter().all(|e| e.attempt().is_some()));
    }

    #[test]
    fn brokkr_test_dedupes_on_projected_features() {
        let oracle = FeatureOracle::fixed(fixture::piners());
        let a = featured("a", &PINERS, &["piners-vm/opcode-counts"]);
        let b = featured(
            "b",
            &PINERS,
            &["piners-vm/opcode-counts", "piners-harness/bench"],
        );
        let sweeps = [a, b];
        let sel = PhaseSelection::for_brokkr_test(
            &sweeps,
            "piners-vm",
            PackageSource::Cli,
            &[],
            &debug_of(false),
            &oracle,
        )
        .unwrap();
        assert!(
            matches!(sel.entry(LaneIdx::new(1)), Some(LaneEntry::Deduped(d)) if d.onto() == LaneIdx::new(0))
        );
        // The deduped lane's own drop is still said: folding it onto `a`
        // decides who runs, not whether its narrowing is announced.
        assert_eq!(
            feature_drop_lines(&[(&sweeps[..], &sel)]),
            s(&[
                "sweep b, -p piners-vm: dropped piners-harness/bench (applies to piners-harness only)"
            ])
        );
    }

    #[test]
    fn brokkr_test_never_dedupes_an_excluded_lane() {
        let sweeps = vec![excluding("excl", &["a"]), sweep("plain")];
        let sel = for_brokkr_test(&sweeps, "a", PackageSource::Cli, &debug_of(false)).unwrap();
        assert!(matches!(
            sel.entry(LaneIdx::new(0)),
            Some(LaneEntry::Excluded(_))
        ));
        assert!(sel.attempt(LaneIdx::new(1)).is_some());
    }

    // Gate-only by default: per-member package-mode resolves are priced for
    // the pre-landing run, not the edit loop.
    #[test]
    fn the_member_selection_follows_its_cadence() {
        use crate::config::{Cadence, MemberCheckConfig, MemberProfile, MemberTarget};
        let cfg = |when| MemberCheckConfig {
            when,
            targets: vec![MemberTarget::Lib, MemberTarget::Bins],
            profile: MemberProfile::Dev,
            skip: Vec::new(),
            only: None,
        };
        let runs = |c: &MemberCheckConfig, certifies, enabled| {
            matches!(
                MemberSelection::build(Some(c), &[], certifies, enabled),
                MemberSelection::Eligible { .. }
            )
        };
        assert!(runs(&cfg(Cadence::Gate), Some(Certifies::Complete), true));
        assert!(!runs(&cfg(Cadence::Gate), Some(Certifies::Partial), true));
        assert!(!runs(&cfg(Cadence::Gate), None, true));
        assert!(runs(&cfg(Cadence::Always), None, true));
        assert!(!runs(&cfg(Cadence::Off), Some(Certifies::Complete), true));
        assert!(!runs(&cfg(Cadence::Always), None, false));
        assert!(matches!(
            MemberSelection::build(None, &[], Some(Certifies::Complete), true),
            MemberSelection::NotApplicable(_)
        ));
    }

    #[test]
    fn the_ledger_lists_the_union_of_both_groups_in_sweep_order() {
        let sweeps = vec![sweep("a"), sweep("b"), sweep("c")];
        let mut ledger = ReachLedger::new(&sweeps);
        assert!(ledger.reached_labels(&sweeps).is_empty());
        ledger.reach_diagnostics(LaneIdx::new(0));
        ledger.reach_test(LaneIdx::new(1));
        assert_eq!(ledger.reached_labels(&sweeps), vec!["a", "b"]);
        ledger.reach_test(LaneIdx::new(0));
        ledger.reach_diagnostics(LaneIdx::new(2));
        assert_eq!(ledger.reached_labels(&sweeps), vec!["a", "b", "c"]);
    }
}
