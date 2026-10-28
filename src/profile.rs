//! Profile resolution for `[test.profiles.*]`.
//!
//! Translates a named profile (with optional `extends` chain) into a
//! list of `SweepDecl`s ready for invocation planning in `brokkr check` (and
//! `brokkr test`) to execute. Each resolved sweep carries:
//!
//! - the `[[check]]` entry that supplies feature flags + build_packages
//! - typed target and libtest filters derived from the merged profile fields
//!   (`tests` / `only` / `skip` / `include_ignored`)
//! - one resolved lane execution mode, including the thread policy
//! - any env vars the profile exports
//!
//! The resolver is intentionally pure-data: it does not run cargo, does
//! not touch disk, and does not depend on `Project`.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use crate::config::ParallelBinaries;
use crate::config::check::{CargoUnification, EffectiveUnification, rustflags_target_key};
use crate::config::{
    CheckEntry, FeatureUnification, Harness, Isolation, ProfileDef, SkipSpec, SweepProfile,
    TestConfig,
};
use crate::error::DevError;

/// One sweep declaration after profile resolution + check-entry lookup.
/// Invocation-specific decisions are added by [`PlannedSweep`].
#[derive(Debug, Clone, Default)]
pub struct SweepDecl {
    /// Display label - the `[[check]]` entry's `name`.
    pub label: String,
    /// The sweep's feature configuration, structured. Deliberately NOT an
    /// argv: what cargo is handed depends on the package selection each run
    /// resolves (a `-p` narrowing, a package-mode resolution, a support
    /// build), so the executable `--features` list is computed per run by
    /// the build-purpose selection resolver and nowhere else. See
    /// [`FeatureConfig`].
    pub features: FeatureConfig,
    /// Packages to rebuild before running tests, sourced from the resolved
    /// `[[check]]` entry. The build uses this sweep's resolved profile and
    /// feature args.
    pub build_packages: Vec<String>,
    /// Packages to scope the clippy / test invocation to, emitted as
    /// `-p <pkg>`. From the `[[check]]` entry's `packages`. Needed for
    /// `--features` to be valid in a virtual workspace.
    pub packages: Vec<String>,
    /// Packages to omit from the test invocation, emitted as
    /// `--workspace --exclude <pkg>`. From the `[[check]]` entry's
    /// `test_exclude_packages`. Test phase only; clippy stays workspace-wide.
    pub test_exclude_packages: Vec<String>,
    /// Canonical typed selection. Launchers render argv only at the boundary.
    pub filters: LaneFilters,
    /// Env vars to export to the cargo subprocess.
    pub env: BTreeMap<String, String>,
    /// Extra `rustc` flags appended to `RUSTFLAGS` for this sweep's cargo runs.
    /// A non-empty value also auto-isolates the sweep's target dir; the
    /// execution layer (`sweep_runtime_env`) turns these into the `RUSTFLAGS`
    /// and `CARGO_TARGET_DIR` env pair. From the `[[check]]` entry's `rustflags`.
    pub rustflags: Vec<String>,
    /// Resolved execution policy, including the thread policy where applicable.
    pub lane_exec: LaneExec,
    /// From the `[[check]]` entry's `curated`: the entry runs a hand-picked
    /// subset, and its shape's non-run pairs are exempt from the coverage
    /// universe. Audit policy only - never part of the build shape, and the
    /// audit exempts a shape only when *every* sweep producing it is curated.
    pub curated: bool,
    /// The `[[check]]` entry's `profile`: the cargo profile this sweep
    /// compiles and runs under, or `None` for the command's default (dev
    /// under `brokkr check`; the CLI/`[test] debug` answer under `brokkr
    /// test`). Part of the build shape.
    pub profile: Option<SweepProfile>,
    /// The `[[check]]` entry's `target`: the triple every cargo run of the
    /// sweep passes as `--target`, or `None` for the host. Part of the build
    /// shape.
    pub target: Option<String>,
    /// The `[[check]]` entry's `feature_unification`, as configured. Policy,
    /// not yet an answer: `auto` resolves differently depending on CLI scope
    /// and parallel eligibility, which are invocation facts.
    pub feature_unification: FeatureUnification,
    /// The bare `[[check]]` entry name, NEVER lane-qualified - `label` becomes
    /// `tier1/default` under a lanes profile, this stays `default`. The key
    /// for state that belongs to the entry rather than the run, today the
    /// parallel timing store: keyed by label, `default` and `tier1/default`
    /// warmed separately and the gate re-paid the whole count-weighted
    /// warm-up minutes after plain check had converged on the identical
    /// entry (measured on the consuming config).
    pub entry_name: String,
    /// Lint the library target alone (`--lib`) instead of `--all-targets`.
    ///
    /// Set ONLY by the investigative `brokkr clippy --lib`; no `[[check]]` key
    /// reaches it, and a gate sweep is always `--all-targets`. It exists because
    /// a whole lint class is invisible under `--all-targets`: an import used
    /// solely from `#[cfg(test)]` code is live in the test target, so
    /// `unused_imports` never fires, while the plain `cargo build`/`cargo doc`
    /// lib build rejects it. Without this flag that class is reproducible only
    /// by the phase that happened to catch it - which is not a probe.
    ///
    /// Deliberately absent from the build shape key: the key
    /// serves clippy/rustdoc dedupe and coverage grouping, all of which only
    /// ever see gate sweeps, and the one sweep that can carry this
    /// runs alone under `cmd_clippy` with nothing to dedupe against.
    pub lib_only: bool,
    /// Where the sweep came from: a configured `[[check]]` entry, or the
    /// doctest carrier a complete claim derived for one. Typed rather than
    /// read off the label, since any printable label is a legal entry name.
    pub origin: SweepOrigin,
}

/// Who declared a sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SweepOrigin {
    /// A `[[check]]` entry (or the ad-hoc / legacy default sweep).
    #[default]
    Configured,
    /// The doctest carrier a `certifies = "complete"` run derived for a
    /// tested build its configured lanes leave without one: `cargo test
    /// --doc` of exactly that lane's requests. `source` is the label of the
    /// lane it was derived from.
    DoctestCarrier { source: String },
}

impl SweepOrigin {
    pub fn is_derived_carrier(&self) -> bool {
        matches!(self, Self::DoctestCarrier { .. })
    }
}

/// A sweep ready for execution in one invocation. Construction resolves the
/// configured unification policy exactly once, after the invocation's scope
/// and parallel eligibility are known. There is deliberately no `Default`:
/// every planned sweep has crossed that resolution boundary.
#[derive(Debug, Clone)]
pub struct PlannedSweep {
    declaration: SweepDecl,
    effective_unification: EffectiveUnification,
}

impl std::ops::Deref for PlannedSweep {
    type Target = SweepDecl;

    fn deref(&self) -> &Self::Target {
        &self.declaration
    }
}

impl PlannedSweep {
    pub fn resolve(declaration: SweepDecl, promotion_eligible: bool) -> Self {
        crate::check_cmd::resolve_planned_sweep(declaration, promotion_eligible)
    }

    /// Construct a planned sweep after invocation selection has resolved its
    /// effective feature-unification answer.
    pub(crate) fn with_effective_unification(
        declaration: SweepDecl,
        effective_unification: EffectiveUnification,
    ) -> Self {
        Self {
            declaration,
            effective_unification,
        }
    }

    pub fn effective_unification(&self) -> EffectiveUnification {
        self.effective_unification
    }

    /// This sweep with its target selection replaced and everything else as
    /// planned - a focused run's projection of the selection onto its one
    /// package.
    pub(crate) fn with_targets(&self, targets: Vec<TargetSelector>) -> Self {
        let mut declaration = self.declaration.clone();
        declaration.filters.targets = targets;
        Self {
            declaration,
            effective_unification: self.effective_unification,
        }
    }

    /// The doctest carrier of this lane: the same planned compile inputs -
    /// selection, features, unification answer, profile, target, rustflags,
    /// env and support builds - executed as `cargo test --doc` under the
    /// watchdog, with every libtest-level filter dropped (doctests cannot be
    /// enumerated, so a filter on them could never be audited). Cloned from
    /// the planned sweep rather than re-resolved, so the unification answer
    /// the invocation gave this lane is the carrier's too.
    pub(crate) fn doctest_carrier(&self, label: String) -> Self {
        let mut declaration = self.declaration.clone();
        declaration.origin = SweepOrigin::DoctestCarrier {
            source: self.label.clone(),
        };
        declaration.label = label;
        declaration.lane_exec = LaneExec::DocOnly {
            threads: Threads::Watchdogged,
        };
        declaration.filters = LaneFilters::default();
        declaration.curated = false;
        Self {
            declaration,
            effective_unification: self.effective_unification,
        }
    }

    /// The profile cargo uses when this sweep leaves the key unset.
    pub(crate) fn cargo_profile(&self) -> SweepProfile {
        crate::build_purpose::profile_for_sweep(
            crate::build_purpose::SweepPurpose::Check,
            crate::build_purpose::SweepProfiles {
                sweep: self.profile,
                ..Default::default()
            },
        )
    }

    /// The declaration this sweep was planned from, for fixtures that revise
    /// one field of an existing sweep.
    #[cfg(test)]
    pub fn declaration(&self) -> &SweepDecl {
        &self.declaration
    }

    /// Consume the sweep, returning the declaration it was planned from.
    #[cfg(test)]
    pub fn into_declaration(self) -> SweepDecl {
        self.declaration
    }
}

/// Test fixtures: a declaration planned as a non-promotable lane. Production
/// code goes through [`PlannedSweep::resolve`], which is told whether the
/// invocation makes the lane promotion-eligible.
#[cfg(test)]
impl From<SweepDecl> for PlannedSweep {
    fn from(declaration: SweepDecl) -> Self {
        Self::resolve(declaration, false)
    }
}

/// libtest thread policy for a cargo-mediated harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Threads {
    /// The watchdog owns one test at a time.
    #[default]
    Watchdogged,
    /// `test_threads = 0`: use libtest's default count.
    CargoDefault,
    /// An explicit libtest count greater than one.
    Cargo(NonZeroU32),
}

impl Threads {
    fn from_config(value: Option<u32>) -> Self {
        match value {
            None | Some(1) => Self::Watchdogged,
            Some(0) => Self::CargoDefault,
            Some(n) => NonZeroU32::new(n).map_or(Self::Watchdogged, Self::Cargo),
        }
    }

    /// Whether concurrency is delegated to libtest inside cargo's shared
    /// output stream, where per-test attribution is unavailable.
    pub fn is_cargo_mediated(self) -> bool {
        !matches!(self, Self::Watchdogged)
    }

    /// Configured libtest count, preserving the special zero meaning.
    pub fn configured_count(self) -> Option<u32> {
        match self {
            Self::Watchdogged => None,
            Self::CargoDefault => Some(0),
            Self::Cargo(n) => Some(n.get()),
        }
    }
}

/// The one resolved execution mode of a sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneExec {
    /// One libtest harness at a time, with the watchdog unless threads are
    /// delegated to libtest.
    Serial { threads: Threads },
    /// Several binaries run concurrently under brokkr's in-flight budget.
    Parallel { budget: u32 },
    /// Each selected test runs in its own cargo invocation.
    Isolated,
    /// Nextest owns process-per-test execution and its concurrency limit.
    Nextest { threads: Option<u32> },
    /// Cargo's doctest pseudo-target, with no test binaries.
    DocOnly { threads: Threads },
}

impl Default for LaneExec {
    fn default() -> Self {
        Self::Serial {
            threads: Threads::Watchdogged,
        }
    }
}

impl LaneExec {
    fn resolve(
        label: &str,
        harness: Harness,
        doc_only: bool,
        parallel_budget: Option<u32>,
        process_isolation: bool,
        configured_threads: Option<u32>,
    ) -> Result<Self, DevError> {
        if doc_only
            && (parallel_budget.is_some() || process_isolation || harness == Harness::Nextest)
        {
            return Err(DevError::Config(format!(
                "sweep '{label}' combines doc-only execution with another lane execution mode"
            )));
        }
        if harness == Harness::Nextest && process_isolation {
            return Err(DevError::Config(format!(
                "sweep '{label}' combines nextest with `isolation = \"process\"`; \
                 nextest already runs process-per-test"
            )));
        }
        if harness == Harness::Nextest && parallel_budget.is_some() {
            return Err(DevError::Config(format!(
                "sweep '{label}' combines nextest with a parallel-binaries budget"
            )));
        }
        if parallel_budget.is_some() && process_isolation {
            return Err(DevError::Config(format!(
                "sweep '{label}' combines process isolation with a parallel-binaries budget"
            )));
        }

        let threads = Threads::from_config(configured_threads);
        if process_isolation && threads.is_cargo_mediated() {
            return Err(DevError::Config(format!(
                "sweep '{label}' combines `isolation = \"process\"` with `test_threads = {}`, \
                 but isolated execution is serial by construction",
                configured_threads.unwrap_or_default()
            )));
        }

        Ok(if doc_only {
            Self::DocOnly { threads }
        } else if harness == Harness::Nextest {
            Self::Nextest {
                threads: configured_threads,
            }
        } else if let Some(budget) = parallel_budget {
            Self::Parallel { budget }
        } else if process_isolation {
            Self::Isolated
        } else {
            Self::Serial { threads }
        })
    }

    /// Whether this execution can apply package-qualified skips because it
    /// attributes tests to individual processes and packages.
    pub fn applies_qualified_skips(self) -> bool {
        matches!(self, Self::Isolated | Self::Nextest { .. })
    }

    /// Whether this execution runs the enumerated test binaries itself,
    /// rather than handing cargo a selection - the lanes that can drop a
    /// binary after the build (`exclude_tests`).
    pub fn runs_enumerated_binaries(self) -> bool {
        matches!(
            self,
            Self::Parallel { .. } | Self::Isolated | Self::Nextest { .. }
        )
    }

    /// The doctest-only lane (`[[check]] doc_only`): `cargo test --doc` and
    /// nothing else. Execution policy, never part of the build shape, and
    /// outside the coverage pair audit (doctests cannot be enumerated).
    pub fn is_doc_only(self) -> bool {
        matches!(self, Self::DocOnly { .. })
    }

    /// The watchdogged doctest-only lane, as a test fixture builds it.
    #[cfg(test)]
    pub fn doc_only() -> Self {
        Self::DocOnly {
            threads: Threads::Watchdogged,
        }
    }

    /// Cargo-mediated thread policy, where applicable.
    pub fn threads(self) -> Option<Threads> {
        match self {
            Self::Serial { threads } | Self::DocOnly { threads } => Some(threads),
            _ => None,
        }
    }
}

/// Whether a sweep shares cargo's target directory or owns an isolated one.
/// The key is also the sole source of the isolated directory's stable name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetLocation {
    Shared,
    Isolated(IsolatedTargetKey),
}

/// The input that makes a sweep's build incompatible with cargo's shared
/// cache. `Rustflags` preserves the established directory name; package
/// unification qualifies that hash because it resolves a different graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IsolatedTargetKey {
    Rustflags(String),
    Unify {
        mode: CargoUnification,
        rustflags_hash: Option<String>,
    },
}

impl TargetLocation {
    pub(crate) fn is_isolated(&self) -> bool {
        matches!(self, Self::Isolated(_))
    }

    /// The cargo target dir this location denotes, using cargo metadata's
    /// resolved directory as the base for both shared and isolated builds.
    pub(crate) fn target_dir(&self, meta_target_dir: &Path) -> PathBuf {
        match self {
            Self::Shared => meta_target_dir.to_path_buf(),
            Self::Isolated(key) => meta_target_dir.join(key.directory_name()),
        }
    }
}

impl IsolatedTargetKey {
    fn directory_name(&self) -> String {
        match self {
            Self::Rustflags(hash) => {
                format!("{}{hash}", crate::clean::layout::RUSTFLAGS_TARGET_PREFIX)
            }
            Self::Unify {
                mode,
                rustflags_hash,
            } => match rustflags_hash {
                Some(hash) => format!(
                    "{}{}-{hash}",
                    crate::target_namespace::UNIFY_PREFIX,
                    mode.as_str()
                ),
                None => format!("{}{}", crate::target_namespace::UNIFY_PREFIX, mode.as_str()),
            },
        }
    }
}

/// A sweep's feature configuration as declared, before any run projects it
/// onto the packages that run selects.
///
/// `tokens` are the `--features` entries one by one (a configured
/// `"a,b c"` is three tokens: cargo splits on commas and whitespace, and so
/// does [`FeatureConfig::split`]). `invocation_explicit` marks features the
/// INVOCATION named (`brokkr check --features`, `brokkr clippy`'s ad-hoc
/// sweeps): those are never projected - the user asked cargo for exactly
/// them, and a token cargo rejects is the user's to see.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct FeatureConfig {
    pub tokens: Vec<String>,
    pub no_default_features: bool,
    pub all_features: bool,
    pub invocation_explicit: bool,
}

impl FeatureConfig {
    /// Split configured feature strings into tokens, cargo's way: commas and
    /// whitespace both separate, and empty pieces are not tokens.
    pub fn split(features: &[String]) -> Vec<String> {
        features
            .iter()
            .flat_map(|f| f.split(|c: char| c == ',' || c.is_whitespace()))
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// A `[[check]]` entry's features.
    pub fn from_entry(entry: &CheckEntry) -> Self {
        Self {
            tokens: Self::split(&entry.features),
            no_default_features: entry.no_default_features,
            all_features: false,
            invocation_explicit: false,
        }
    }

    /// Features the invocation named itself: never projected.
    pub fn explicit(features: &[String], no_default_features: bool, all_features: bool) -> Self {
        Self {
            tokens: Self::split(features),
            no_default_features,
            all_features,
            invocation_explicit: true,
        }
    }

    /// The cargo argv for this configuration with `tokens` as the feature
    /// list - the configured tokens, or a run's projection of them. No
    /// `--features` at all when `tokens` is empty.
    pub fn argv_with(&self, tokens: &[String]) -> Vec<String> {
        let mut args = Vec::new();
        if self.all_features {
            args.push("--all-features".into());
        }
        if self.no_default_features {
            args.push("--no-default-features".into());
        }
        if !tokens.is_empty() {
            args.push("--features".into());
            args.push(tokens.join(","));
        }
        args
    }

    /// The configured policy as a key: the flags and every token, in order.
    /// Identifies the DECLARED sweep (coverage grouping); never what a run
    /// compiles, which is the projection's business.
    pub fn configured_key(&self) -> Vec<String> {
        self.argv_with(&self.tokens)
    }
}

/// Which half of the filter surface a [`Filter`] is, because the two are dead
/// for different reasons and are checked against different sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterKind {
    /// A `skip` entry: dead when nothing it could remove exists.
    Skip,
    /// An `only` entry: dead when the lane evaluates nothing under it.
    Only,
}

impl FilterKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FilterKind::Skip => "skip",
            FilterKind::Only => "only",
        }
    }
}

/// Config address for a filter's declaration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FilterOrigin {
    Profile(String),
    Entry(String),
}

impl FilterOrigin {
    /// Render the config block named by this provenance.
    pub fn display(&self) -> String {
        match self {
            Self::Profile(name) => format!("[test.profiles.{name}]"),
            Self::Entry(name) => format!("[[check]] '{name}'"),
        }
    }
}

/// One `skip` / `only` filter with typed provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub kind: FilterKind,
    /// The test-name substring, as written.
    pub pattern: String,
    /// Set on a package-qualified skip; the pattern then applies within this
    /// package only, exactly as a `[[quarantine]]` entry's `package` scopes it.
    pub package: Option<String>,
    /// The profile or check entry that declared this filter.
    pub origin: FilterOrigin,
}

impl Filter {
    /// Does this filter match `test` in `package`? Substring semantics, the
    /// same libtest applies to both `--skip` and a positional filter, with the
    /// package scoping that only a qualified skip carries.
    pub fn matches(&self, package: &str, test: &str) -> bool {
        self.package.as_deref().is_none_or(|p| p == package) && test.contains(&self.pattern)
    }

    /// How the dead-filter report names it.
    pub fn label(&self) -> String {
        match &self.package {
            Some(pkg) => format!(
                "{} \"{}\" (package {pkg})",
                self.kind.as_str(),
                self.pattern
            ),
            None => format!("{} \"{}\"", self.kind.as_str(), self.pattern),
        }
    }
}

/// Compatibility name used by the check phase while its call sites migrate.
pub type DeclaredFilter = Filter;

/// A check target selected through `[[check]].tests` or a profile's `tests`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetName(String);

impl TargetName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One cargo target selector a sweep declares. Selectors union, as cargo's
/// target flags do: `--lib --test cli` is the library harness plus `cli`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TargetSelector {
    /// `lib_tests = true`: cargo `--lib`, every selected package's library
    /// unit-test harness, with cargo's explicit-selection semantics (a
    /// library whose manifest sets `test = false` is selected too).
    Lib,
    /// One `tests` entry: cargo `--test <name>`, a glob as cargo allows.
    Test(TargetName),
}

impl TargetSelector {
    pub(crate) fn cargo_args(&self) -> Vec<String> {
        match self {
            Self::Lib => vec!["--lib".to_owned()],
            Self::Test(name) => vec!["--test".to_owned(), name.0.clone()],
        }
    }

    /// Whether `package` has a target this selector names - the condition
    /// for handing the selector to a cargo run scoped to that one package,
    /// which refuses a selector naming nothing it has.
    pub(crate) fn selects_in(&self, package: &crate::cargo::metadata::CargoPackage) -> bool {
        const LIB_KINDS: [&str; 6] = ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];
        package.targets.iter().any(|t| match self {
            Self::Lib => t.kind.iter().any(|k| LIB_KINDS.contains(&k.as_str())),
            Self::Test(name) => {
                t.kind.iter().any(|k| k == "test") && target_glob_matches(name.as_str(), &t.name)
            }
        })
    }

    /// How the plan line names it.
    pub fn describe(&self) -> String {
        match self {
            Self::Lib => "--lib".to_owned(),
            Self::Test(name) => format!("--test {}", name.0),
        }
    }
}

/// Whether `target` matches a target-name `pattern`, with cargo's glob
/// support. A pattern that is not a valid glob is compared literally.
pub(crate) fn target_glob_matches(pattern: &str, target: &str) -> bool {
    globset::Glob::new(pattern).map_or(pattern == target, |g| g.compile_matcher().is_match(target))
}

/// Refuse an `exclude_tests` the sweep's lane cannot honour. A serial lane
/// hands cargo its selection and cargo has no complement selector, so the
/// excluded targets would run - refused, never dropped.
pub(crate) fn refuse_unhonoured_exclusions(decl: &SweepDecl) -> Result<(), DevError> {
    if decl.filters.exclude.is_empty() || decl.lane_exec.runs_enumerated_binaries() {
        return Ok(());
    }
    Err(DevError::Config(format!(
        "sweep '{}' has `exclude_tests`, but its tests run through one `cargo test` \
         invocation, which cannot leave a target out. Exclusion needs a lane that runs its \
         test binaries itself: set `parallel = {{}}` or `harness = \"nextest\"` on the \
         [[check]] entry, or `isolation = \"process\"` on the profile.",
        decl.label
    )))
}

/// The `[[check]]` entry's own target selection: its `tests`, plus the
/// library harness when it sets `lib_tests`.
fn entry_selectors(entry: &CheckEntry) -> Vec<TargetSelector> {
    let mut out: Vec<TargetSelector> = entry
        .tests
        .iter()
        .cloned()
        .map(|t| TargetSelector::Test(TargetName::new(t)))
        .collect();
    if entry.lib_tests {
        out.push(TargetSelector::Lib);
    }
    out
}

/// Semantic filters and target selection for one resolved sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaneFilters {
    /// Ordered skips and positional filters, each retaining provenance.
    pub filters: Vec<Filter>,
    /// Cargo target selectors (`tests`, `lib_tests`); empty selects every
    /// test target.
    pub targets: Vec<TargetSelector>,
    /// `exclude_tests`: integration-test targets (globs) removed from what
    /// the lane executes after the build. Never cargo argv - cargo has no
    /// complement selector, and spelling the complement out as explicit
    /// selectors would change what cargo builds and refuses - so only lanes
    /// that execute their enumerated binaries themselves can honour it
    /// (`LaneExec::runs_enumerated_binaries`).
    pub exclude: Vec<TargetName>,
    /// Whether this lane executes ignored tests before forwarded args are
    /// considered.
    pub include_ignored: bool,
}

impl LaneFilters {
    pub fn skips(&self) -> impl Iterator<Item = &Filter> {
        self.filters.iter().filter(|f| f.kind == FilterKind::Skip)
    }

    pub fn only(&self) -> impl Iterator<Item = &Filter> {
        self.filters.iter().filter(|f| f.kind == FilterKind::Only)
    }

    /// Whether the complete launch argv executes ignored tests.
    pub fn runs_ignored_with<'a>(&self, extra: impl IntoIterator<Item = &'a str>) -> bool {
        if self.include_ignored {
            return true;
        }
        let mut args = extra.into_iter();
        while let Some(arg) = args.next() {
            match arg {
                "--skip" => {
                    args.next();
                }
                "--include-ignored" | "--ignored" => return true,
                _ => {}
            }
        }
        false
    }

    /// Render cargo's target selectors at the launch boundary.
    pub fn cargo_args(&self) -> Vec<String> {
        self.targets
            .iter()
            .flat_map(TargetSelector::cargo_args)
            .collect()
    }

    /// Whether `exclude_tests` removes the integration-test target `name`.
    /// Only `test`-kind targets can be excluded; `kind` is the target's
    /// selector kind (`test`, `lib`, `bin`, ...).
    pub fn excludes(&self, kind: &str, name: &str) -> bool {
        kind == "test"
            && self
                .exclude
                .iter()
                .any(|e| target_glob_matches(e.as_str(), name))
    }

    /// Render libtest's configured `--skip` flags and ignored-test switch.
    pub fn libtest_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.include_ignored {
            out.push("--include-ignored".into());
        }
        for filter in self.skips().filter(|f| f.package.is_none()) {
            out.push("--skip".into());
            out.push(filter.pattern.clone());
        }
        out
    }

    /// Render positional filters passed after `--` to libtest.
    pub fn name_filters(&self) -> Vec<String> {
        self.only().map(|filter| filter.pattern.clone()).collect()
    }
}

impl SweepDecl {
    /// The resolved execution policy.
    pub fn lane_exec(&self) -> LaneExec {
        self.lane_exec
    }

    /// The sweep's semantic filters.
    pub fn lane_filters(&self) -> &LaneFilters {
        &self.filters
    }

    /// Filter argv in order for the libtest (post-`--`) section.
    /// Helper for tests.
    #[cfg(test)]
    pub fn libtest_argv(&self) -> Vec<String> {
        let mut out = self.filters.libtest_args();
        for n in self.filters.name_filters() {
            out.push(n);
        }
        out
    }
}

impl PlannedSweep {
    /// The sweep's *build shape*: everything that decides what cargo
    /// compiles, and nothing that only decides which tests run. The effective
    /// unification answer belongs here because it depends on invocation scope.
    #[cfg(test)]
    pub fn build_shape_key(&self) -> BuildShapeKey {
        (
            self.packages.clone(),
            self.features.configured_key(),
            self.rustflags.clone(),
            self.env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            self.build_packages.clone(),
            self.profile,
            self.effective_unification,
        )
    }

    /// The one target-location decision for this sweep. Its isolated key
    /// determines both whether the caller needs cargo's metadata target dir
    /// and the directory path built phases use.
    ///
    /// Isolation exists to stop two builds that cannot share a cache from
    /// alternating in one directory and rebuilding each other: `rustflags`
    /// re-fingerprint every unit, and package-mode unification resolves a
    /// different feature graph for the same source. Pinned `selected` and
    /// `workspace` deliberately do NOT isolate - ambient resolution is already
    /// one of those in any given repo, so isolating them would split the common
    /// cache for no gain. Package mode shares ONE directory across its
    /// per-package resolutions, because a lane exports a single
    /// `BROKKR_TEST_BIN_DIR`.
    pub(crate) fn target_location(&self) -> TargetLocation {
        let rustflags_hash = rustflags_target_key(&self.rustflags);
        if self.effective_unification.is_per_package() {
            TargetLocation::Isolated(IsolatedTargetKey::Unify {
                mode: CargoUnification::Package,
                rustflags_hash,
            })
        } else if let Some(hash) = rustflags_hash {
            TargetLocation::Isolated(IsolatedTargetKey::Rustflags(hash))
        } else {
            TargetLocation::Shared
        }
    }

    /// The cargo argv fragment pinning this sweep's resolution, empty under
    /// `auto` where nothing is pinned. Every cargo invocation derived from the
    /// sweep must carry it - clippy, the pre-build, the test run, the parallel
    /// prebuild and its per-binary re-entries, the isolation lane, and the
    /// coverage enumeration - or the audit describes a build that never ran.
    pub fn unification_args(&self) -> Vec<String> {
        self.effective_unification.cargo_args()
    }
}

/// The tuple `PlannedSweep::build_shape_key` compares.
#[cfg(test)]
type BuildShapeKey = (
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<(String, String)>,
    Vec<String>,
    Option<SweepProfile>,
    EffectiveUnification,
);

/// Synthesize a `SweepDecl` from a `CheckEntry` alone, with no
/// profile filters. Used by the bare `brokkr check` path when
/// `[[check]]` is configured but no profile is selected.
pub fn sweep_from_check_entry(entry: &CheckEntry) -> SweepDecl {
    // No profile in play, so the entry's own filters are all there is.
    let filters = LaneFilters {
        filters: entry_filters(entry),
        targets: entry_selectors(entry),
        exclude: entry
            .exclude_tests
            .iter()
            .cloned()
            .map(TargetName::new)
            .collect(),
        include_ignored: false,
    };
    let lane_exec = LaneExec::resolve(
        &entry.name,
        entry.harness,
        entry.doc_only,
        entry.parallel.map(ParallelBinaries::resolved_budget),
        false,
        None,
    )
    .expect("parsed check entries have a valid execution mode");

    sweep_decl_from_entry(entry, filters, entry.env.clone(), lane_exec)
}

/// The `[[check]]` entry's shared sweep fields, with profile resolution
/// supplying only its filter/env overlays and execution policy.
fn sweep_decl_from_entry(
    entry: &CheckEntry,
    filters: LaneFilters,
    env: BTreeMap<String, String>,
    lane_exec: LaneExec,
) -> SweepDecl {
    SweepDecl {
        label: entry.name.clone(),
        features: FeatureConfig::from_entry(entry),
        build_packages: entry.build_packages.clone(),
        packages: entry.packages.clone(),
        test_exclude_packages: entry.test_exclude_packages.clone(),
        filters,
        env,
        rustflags: entry.rustflags.clone(),
        lane_exec,
        curated: entry.curated,
        profile: entry.profile,
        target: entry.target.clone(),
        feature_unification: entry.feature_unification,
        entry_name: entry.name.clone(),
        // A gate sweep always lints every target; only the investigative
        // `brokkr clippy --lib` narrows, and it sets this after construction.
        lib_only: false,
        origin: SweepOrigin::Configured,
    }
}

/// The `[[check]]` entry's own filters, provenance attached.
fn entry_filters(entry: &CheckEntry) -> Vec<DeclaredFilter> {
    let origin = FilterOrigin::Entry(entry.name.clone());
    let skips = entry.skip.iter().map(|s| DeclaredFilter {
        kind: FilterKind::Skip,
        pattern: s.clone(),
        package: None,
        origin: origin.clone(),
    });
    let onlys = entry.only.iter().map(|o| DeclaredFilter {
        kind: FilterKind::Only,
        pattern: o.clone(),
        package: None,
        origin: origin.clone(),
    });
    skips.chain(onlys).collect()
}

/// Merged, resolved view of a `ProfileDef` after walking its `extends`
/// chain. Collection fields default to empty when nothing was set
/// anywhere in the chain.
#[derive(Debug, Clone, Default)]
struct ResolvedProfile {
    sweeps: Vec<String>,
    tests: Vec<String>,
    lib_tests: bool,
    exclude_tests: Vec<String>,
    only: Vec<String>,
    skip: Vec<SkipSpec>,
    /// Which profile in the `extends` chain actually declared `only` / `skip`.
    /// The merge is whole-list replacement, so each list came from exactly one
    /// def - and a dead-filter report that named the resolved profile when the
    /// filter is written in a parent sends the reader to a block that does not
    /// contain it.
    only_from: Option<String>,
    skip_from: Option<String>,
    include_ignored: bool,
    test_threads: Option<u32>,
    isolation: Option<Isolation>,
    env: BTreeMap<String, String>,
}

/// Resolve `name` into a list of `SweepDecl`s ready to execute.
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
) -> Result<Vec<SweepDecl>, DevError> {
    // A `lanes` profile is a list of runs: each lane resolves on its own
    // (load-time validation guarantees lanes carry no run-shaping fields,
    // don't nest, and exist), concatenated in declaration order. Labels are
    // lane-qualified (`tier1/default`) so the log can tell two runs of the
    // same `[[check]]` entry apart.
    if let Some(def) = cfg.profiles.get(name)
        && let Some(lanes) = &def.lanes
    {
        let mut out = Vec::new();
        for lane in lanes {
            let mut lane_sweeps = resolve_single(cfg, checks, lane)?;
            for s in &mut lane_sweeps {
                s.label = format!("{lane}/{}", s.label);
            }
            out.extend(lane_sweeps);
        }
        return Ok(out);
    }
    resolve_single(cfg, checks, name)
}

/// Validate every runnable profile's resolved lane execution policy. Profile
/// bases with no inherited sweeps are allowed and need no sweep construction;
/// all other profiles go through the same extends and lane resolver used at
/// invocation time.
pub(crate) fn resolve_profiles_for_validation(
    cfg: &TestConfig,
    checks: &[CheckEntry],
) -> Result<BTreeMap<String, Vec<SweepDecl>>, DevError> {
    let mut resolved = BTreeMap::new();
    for name in cfg.profiles.keys() {
        let sweeps = if cfg
            .profiles
            .get(name)
            .is_some_and(|def| def.lanes.is_some())
        {
            resolve(cfg, checks, name)?
        } else if resolve_profile_chain(&cfg.profiles, name)?
            .sweeps
            .is_empty()
        {
            Vec::new()
        } else {
            resolve_single(cfg, checks, name)?
        };
        resolved.insert(name.clone(), sweeps);
    }
    Ok(resolved)
}

/// Resolve one non-`lanes` profile (the pre-lanes `resolve` body).
fn resolve_single(
    cfg: &TestConfig,
    checks: &[CheckEntry],
    name: &str,
) -> Result<Vec<SweepDecl>, DevError> {
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
        out.push(build_sweep_decl(entry, &merged, name)?);
    }

    // Package-qualified skips are filtered out of the enumerated set, and
    // the enumerated set only exists where there is per-package attribution:
    // the process-isolated path, and the nextest lane (whose filterset
    // translation carries the package scope natively). A shared-process
    // libtest run has neither.
    if out.iter().any(|s| {
        s.lane_filters()
            .skips()
            .any(|filter| filter.package.is_some())
            && !s.lane_exec().applies_qualified_skips()
    }) {
        return Err(DevError::Config(format!(
            "[test.profiles.{name}] has package-qualified skip entries; these \
             filter the enumerated set and require `isolation = \"process\"` \
             on this profile."
        )));
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
    for (def_name, def) in chain.iter().rev() {
        if let Some(v) = &def.sweeps {
            out.sweeps = v.clone();
        }
        if let Some(v) = &def.tests {
            out.tests = v.clone();
        }
        // Like every list here, a child's explicit value replaces its
        // parent's - `lib_tests = false` clears an inherited `true`.
        if let Some(v) = def.lib_tests {
            out.lib_tests = v;
        }
        if let Some(v) = &def.exclude_tests {
            out.exclude_tests = v.clone();
        }
        if let Some(v) = &def.only {
            out.only = v.clone();
            out.only_from = Some((*def_name).to_owned());
        }
        if let Some(v) = &def.skip {
            out.skip = v.clone();
            out.skip_from = Some((*def_name).to_owned());
        }
        if let Some(v) = def.include_ignored {
            out.include_ignored = v;
        }
        if let Some(v) = def.test_threads {
            out.test_threads = Some(v);
        }

        if let Some(v) = def.isolation {
            out.isolation = Some(v);
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
) -> Result<Vec<(&'a str, &'a ProfileDef)>, DevError> {
    let mut chain: Vec<(&str, &ProfileDef)> = Vec::new();
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
        let (key, def) = profiles
            .get_key_value(&cur)
            .ok_or_else(|| DevError::Config(format!("[test.profiles.{cur}] is not defined")))?;
        chain.push((key.as_str(), def));
        match &def.extends {
            Some(parent) => cur = parent.clone(),
            None => return Ok(chain),
        }
    }
}

/// The *run-shaping* half of a profile: which tests run and how they are
/// executed, with nothing that decides what cargo compiles.
///
/// Split out of [`build_sweep_decl`] so the ad-hoc CLI-features path can
/// inherit it. A `--features` run overrides *sweep selection* - which
/// `[[check]]` entry's feature shape to compile - and that is orthogonal to
/// the profile's filters: a test that cannot pass in-process cannot pass
/// in-process at any feature shape. Dropping these along with the sweep set
/// was the defect (a bare `cargo test` with no `--skip` at all, reporting a
/// red that reads as a code failure).
///
/// `env` is here rather than in the build shape because a profile's env is
/// profile-wide policy; a `[[check]]` entry's env is not, and an ad-hoc run
/// deliberately inherits no entry.
#[derive(Debug, Clone, Default)]
pub struct RunShaping {
    filters: LaneFilters,
    env: BTreeMap<String, String>,
    configured_threads: Option<u32>,
    process_isolated: bool,
    lane_exec: LaneExec,
}

impl RunShaping {
    /// True when the profile shapes nothing - a `lanes` profile, or one
    /// declaring no filters. Lets the caller say "no filters applied"
    /// rather than name a profile that changed nothing.
    pub fn is_empty(&self) -> bool {
        self.filters == LaneFilters::default()
            && self.env.is_empty()
            && self.configured_threads.is_none()
            && !self.process_isolated
    }

    /// Overlay onto an ad-hoc sweep. Assignment, not merge: an ad-hoc sweep
    /// carries no `[[check]]` entry, so there is nothing to AND with.
    pub fn apply(self, sweep: &mut SweepDecl) {
        sweep.filters = self.filters;
        sweep.env = self.env;
        sweep.lane_exec = self.lane_exec;
    }
}

/// The run shaping `name` contributes, for the ad-hoc CLI-features path.
///
/// A `lanes` profile returns an empty shaping: load-time validation
/// guarantees lanes carry no run-shaping fields of their own, and there is
/// no defensible way to pick one lane's filters for a single ad-hoc run.
///
/// The shaping lands on an ad-hoc sweep, which is always a shared-process
/// libtest lane unless the profile itself sets `isolation = "process"`. So the
/// same rule [`resolve_single`] applies to a profile's own sweeps applies here,
/// and it cannot be inherited from there: a profile may carry
/// package-qualified skips legitimately because every sweep it names is a
/// nextest lane, and handing those skips to an ad-hoc libtest lane - where
/// package-scoped filtering is unavailable - would silently run the tests they
/// exclude. Refused, never dropped.
pub fn run_shaping(cfg: &TestConfig, name: &str) -> Result<RunShaping, DevError> {
    if cfg.profiles.get(name).is_some_and(|d| d.lanes.is_some()) {
        return Ok(RunShaping::default());
    }
    let mut shaping = shaping_from(&resolve_profile_chain(&cfg.profiles, name)?, name);
    let lane_exec = LaneExec::resolve(
        name,
        Harness::Libtest,
        false,
        None,
        shaping.process_isolated,
        shaping.configured_threads,
    )?;
    if shaping
        .filters
        .skips()
        .any(|filter| filter.package.is_some())
        && !lane_exec.applies_qualified_skips()
    {
        return Err(DevError::Config(format!(
            "[test.profiles.{name}] has package-qualified skip entries, and an ad-hoc \
             `--features`/`--no-default-features` run replaces its sweeps with one \
             shared-process libtest lane, which cannot apply a package-scoped skip. \
             Run the profile without ad-hoc features, pick a profile without qualified \
             skips (`--profile`), or set `isolation = \"process\"` on it."
        )));
    }
    shaping.lane_exec = lane_exec;
    Ok(shaping)
}

/// `profile_name` is carried for provenance alone: the merged profile is the
/// product of an `extends` chain and has no name of its own, but a dead-filter
/// report has to name a block the reader can open.
fn shaping_from(profile: &ResolvedProfile, profile_name: &str) -> RunShaping {
    // Keep the configured thread count as resolution input until the sweep's
    // harness and other execution settings are known. The resolved sweep
    // stores only its typed `LaneExec`.
    // Provenance is per list, not per profile: `extends` replaces `only` and
    // `skip` wholesale, so each names the def that actually declared it.
    let skip_at = FilterOrigin::Profile(
        profile
            .skip_from
            .as_deref()
            .unwrap_or(profile_name)
            .to_owned(),
    );
    let only_at = FilterOrigin::Profile(
        profile
            .only_from
            .as_deref()
            .unwrap_or(profile_name)
            .to_owned(),
    );
    let mut filters: Vec<Filter> = Vec::new();
    for spec in &profile.skip {
        let (pattern, package) = match spec {
            SkipSpec::Name(s) => (s.clone(), None),
            SkipSpec::Qualified(q) => (q.pattern.clone(), Some(q.package.clone())),
        };
        filters.push(Filter {
            kind: FilterKind::Skip,
            pattern,
            package,
            origin: skip_at.clone(),
        });
    }
    filters.extend(profile.only.iter().map(|o| Filter {
        kind: FilterKind::Only,
        pattern: o.clone(),
        package: None,
        origin: only_at.clone(),
    }));

    let mut targets: Vec<TargetSelector> = profile
        .tests
        .iter()
        .cloned()
        .map(|t| TargetSelector::Test(TargetName::new(t)))
        .collect();
    if profile.lib_tests {
        targets.push(TargetSelector::Lib);
    }
    let filters = LaneFilters {
        filters,
        targets,
        exclude: profile
            .exclude_tests
            .iter()
            .cloned()
            .map(TargetName::new)
            .collect(),
        include_ignored: profile.include_ignored,
    };
    RunShaping {
        filters,
        env: profile.env.clone(),
        configured_threads: profile.test_threads,
        process_isolated: profile.isolation == Some(Isolation::Process),
        lane_exec: LaneExec::default(),
    }
}

fn build_sweep_decl(
    entry: &CheckEntry,
    profile: &ResolvedProfile,
    profile_name: &str,
) -> Result<SweepDecl, DevError> {
    let shaping = shaping_from(profile, profile_name);
    let mut filters = shaping.filters;
    filters.filters.extend(entry_filters(entry));
    for selector in entry_selectors(entry) {
        if !filters.targets.contains(&selector) {
            filters.targets.push(selector);
        }
    }
    filters
        .exclude
        .extend(entry.exclude_tests.iter().cloned().map(TargetName::new));
    // Profile env is the base; the entry's own env overlays it so a
    // sweep-specific var wins on a key collision.
    let mut env = shaping.env;
    for (k, v) in &entry.env {
        env.insert(k.clone(), v.clone());
    }

    let parallel_budget = entry.parallel.map(ParallelBinaries::resolved_budget);
    let process_isolation = profile.isolation == Some(Isolation::Process);
    let lane_exec = LaneExec::resolve(
        &entry.name,
        entry.harness,
        entry.doc_only,
        parallel_budget,
        process_isolation,
        profile.test_threads,
    )
    .map_err(|error| DevError::Config(format!("[test.profiles.{profile_name}]: {error}")))?;

    Ok(sweep_decl_from_entry(entry, filters, env, lane_exec))
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
    use crate::check_cmd::resolve_unification;

    fn plan_decl(declaration: SweepDecl) -> PlannedSweep {
        PlannedSweep::resolve(declaration, false)
    }

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
    fn resolve_lanes_concatenates_and_qualifies_labels() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "default"

[test.profiles.tier1]
sweeps = ["default"]
skip = ["serial::"]
test_threads = 0

[test.profiles.serial]
sweeps = ["default"]
only = ["serial::"]
test_threads = 1

[test.profiles.pre-commit]
lanes = ["tier1", "serial"]
"#,
        );
        let sweeps = resolve(&cfg, &checks, "pre-commit").unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "tier1/default");
        assert_eq!(sweeps[1].label, "serial/default");
        // Each lane keeps its own filters and thread policy - a lanes
        // profile is a list of runs, not a merge...
        assert_eq!(sweeps[0].libtest_argv(), vec!["--skip", "serial::"]);
        assert_eq!(
            sweeps[0].lane_exec,
            LaneExec::Serial {
                threads: Threads::CargoDefault
            }
        );
        assert_eq!(sweeps[1].libtest_argv(), vec!["serial::"]);
        assert_eq!(
            sweeps[1].lane_exec,
            LaneExec::Serial {
                threads: Threads::Watchdogged
            }
        );
        // ...while the build shape is identical, which is exactly what
        // clippy (and `brokkr test`) dedupe on.
        assert_eq!(
            plan_decl(sweeps[0].clone()).build_shape_key(),
            plan_decl(sweeps[1].clone()).build_shape_key()
        );
    }

    #[test]
    fn lib_selection_inherits_unions_and_clears() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "default"
tests = ["error_boundary"]
exclude_tests = ["slow_*"]
parallel = {}

[test.profiles.base]
sweeps = ["default"]
lib_tests = true
exclude_tests = ["compiler"]

[test.profiles.child]
extends = "base"

[test.profiles.cleared]
extends = "base"
lib_tests = false
"#,
        );
        let lib = TargetSelector::Lib;
        let boundary = TargetSelector::Test(TargetName::new("error_boundary"));
        let child = resolve(&cfg, &checks, "child").unwrap();
        // The inherited lib selection unions with the entry's `tests`, and the
        // exclusions of both levels apply.
        assert_eq!(child[0].filters.targets, [lib, boundary.clone()]);
        assert_eq!(
            child[0].filters.cargo_args(),
            ["--lib", "--test", "error_boundary"]
        );
        let excluded: Vec<&str> = child[0]
            .filters
            .exclude
            .iter()
            .map(TargetName::as_str)
            .collect();
        assert_eq!(excluded, ["compiler", "slow_*"]);
        assert!(child[0].filters.excludes("test", "slow_parse"));
        assert!(
            !child[0].filters.excludes("lib", "slow_parse"),
            "only test targets are excluded"
        );
        // An explicit `false` clears the inherited selection.
        let cleared = resolve(&cfg, &checks, "cleared").unwrap();
        assert_eq!(cleared[0].filters.targets, [boundary]);
    }

    /// A serial lane hands cargo its selection and cargo cannot leave a
    /// target out, so an exclusion there is refused, not dropped.
    #[test]
    fn an_exclusion_on_a_serial_lane_is_refused() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "default"
exclude_tests = ["slow"]

[test.profiles.serial]
sweeps = ["default"]

[test.profiles.isolated]
sweeps = ["default"]
isolation = "process"
"#,
        );
        let err =
            crate::check_cmd::decide_active_sweeps(&checks, Some(&cfg), Some("serial"), &[], false)
                .unwrap_err()
                .to_string();
        assert!(err.contains("exclude_tests"), "got: {err}");
        assert!(
            crate::check_cmd::decide_active_sweeps(
                &checks,
                Some(&cfg),
                Some("isolated"),
                &[],
                false
            )
            .is_ok()
        );
        // No profile at all: the entry alone is a serial lane.
        assert!(crate::check_cmd::decide_active_sweeps(&checks, None, None, &[], false).is_err());
    }

    #[test]
    fn isolation_reaches_the_sweep_and_stays_out_of_the_shape() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "default"

[test.profiles.serial]
sweeps = ["default"]
only = ["serial::"]
isolation = "process"
"#,
        );
        let sweeps = resolve(&cfg, &checks, "serial").unwrap();
        assert_eq!(sweeps[0].lane_exec, LaneExec::Isolated);

        // Execution policy, not build shape: an isolated and a plain sweep
        // of the same entry still dedupe to one clippy run.
        let plain = sweep_from_check_entry(&checks[0]);
        assert_eq!(
            plan_decl(plain).build_shape_key(),
            plan_decl(sweeps[0].clone()).build_shape_key()
        );
    }

    #[test]
    fn ad_hoc_shaping_refuses_qualified_skips_it_cannot_apply() {
        // The profile is legal on its own sweeps (all nextest), but the
        // ad-hoc path hands its shaping to one shared-process libtest lane
        // that cannot apply package-scoped filters - the excluded tests would run.
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "nx"
harness = "nextest"

[test.profiles.p]
sweeps = ["nx"]
skip = [{ package = "a", pattern = "slow" }]
"#,
        );
        assert!(resolve(&cfg, &checks, "p").is_ok());
        let err = run_shaping(&cfg, "p").unwrap_err().to_string();
        assert!(err.contains("package-qualified"), "{err}");

        // Under process isolation the ad-hoc lane enumerates, so the skips
        // are honoured and the shaping goes through.
        let (_, cfg) = parse_fragment(
            r#"
[test.profiles.p]
sweeps = ["x"]
isolation = "process"
skip = [{ package = "a", pattern = "slow" }]
"#,
        );
        assert!(run_shaping(&cfg, "p").is_ok());
    }

    #[test]
    fn unification_mode_is_part_of_the_build_shape() {
        // Two lanes identical in every other field. Package and workspace
        // resolution enable different features, so they compile different
        // source and present different lint surfaces - deduping either into
        // the other would lint one build and run the other, which is the
        // whole argument the coverage phase is built on.
        let base = plan_decl(SweepDecl::default());
        let pkg = plan_decl(SweepDecl {
            feature_unification: FeatureUnification::Package,
            ..base.declaration().clone()
        });
        let ws = plan_decl(SweepDecl {
            feature_unification: FeatureUnification::Workspace,
            ..base.declaration().clone()
        });
        assert_ne!(pkg.build_shape_key(), ws.build_shape_key());
        assert_ne!(pkg.build_shape_key(), base.build_shape_key());
        // And an unpinned lane is not the same shape as one pinned to
        // cargo's default: `Ambient` defers to the config chain, which is a
        // different promise even where it lands on the same answer.
        let sel = plan_decl(SweepDecl {
            feature_unification: FeatureUnification::Selected,
            ..base.declaration().clone()
        });
        assert_ne!(sel.build_shape_key(), base.build_shape_key());
    }

    #[test]
    fn auto_preserves_the_pre_existing_behaviour_in_both_directions() {
        // The compatibility contract: `auto` un-promoted pins nothing at all,
        // which is what every sweep did before this key existed...
        let auto = SweepDecl::default();
        assert_eq!(
            resolve_unification(&auto, false),
            EffectiveUnification::Ambient
        );
        assert!(plan_decl(auto.clone()).unification_args().is_empty());
        // ...and `auto` on a promotion-eligible parallel sweep still pins
        // workspace, which is what the parallel lane did before it.
        assert_eq!(
            resolve_unification(&auto, true),
            EffectiveUnification::Pinned(CargoUnification::Workspace)
        );
    }

    #[test]
    fn explicit_selected_is_honoured_even_on_a_promotion_eligible_sweep() {
        let sweep = SweepDecl {
            label: "lane".into(),
            feature_unification: FeatureUnification::Selected,
            ..SweepDecl::default()
        };
        // This was a refusal when the fan-out re-entered cargo per binary -
        // selected mode let a runner resolve a different graph from its
        // prebuild. Direct execution runs the prebuilt binaries themselves,
        // so the request is deliverable everywhere and is simply pinned.
        assert_eq!(
            resolve_unification(&sweep, true),
            EffectiveUnification::Pinned(CargoUnification::Selected)
        );
        assert_eq!(
            resolve_unification(&sweep, false),
            EffectiveUnification::Pinned(CargoUnification::Selected)
        );
    }

    #[test]
    fn only_package_mode_and_rustflags_isolate_the_target_dir() {
        // The default must keep cargo's own dir, or every existing tree pays
        // a cold rebuild for a feature it does not use.
        assert_eq!(
            plan_decl(SweepDecl::default()).target_location(),
            TargetLocation::Shared
        );
        // A pinned selected/workspace sweep resolves what ambient already
        // resolves in any given repo, so splitting the cache buys nothing.
        for mode in [CargoUnification::Selected, CargoUnification::Workspace] {
            assert_eq!(
                plan_decl(SweepDecl {
                    feature_unification: match mode {
                        CargoUnification::Selected => FeatureUnification::Selected,
                        CargoUnification::Workspace => FeatureUnification::Workspace,
                        CargoUnification::Package => FeatureUnification::Package,
                    },
                    ..SweepDecl::default()
                })
                .target_location(),
                TargetLocation::Shared,
            );
        }
        // Package mode isolates even with no rustflags: it resolves a
        // different graph and would otherwise thrash against every ordinary
        // sweep, in both directions, on every run.
        let pkg = plan_decl(SweepDecl {
            feature_unification: FeatureUnification::Package,
            ..SweepDecl::default()
        });
        assert_eq!(
            pkg.target_location(),
            TargetLocation::Isolated(IsolatedTargetKey::Unify {
                mode: CargoUnification::Package,
                rustflags_hash: None,
            })
        );
        assert_eq!(
            pkg.target_location().target_dir(Path::new("/target")),
            PathBuf::from("/target/unify-package")
        );

        // The rustflags-only name is unchanged, so no existing isolated dir
        // is re-derived by this feature.
        let flags = vec!["--cfg".to_owned(), "madsim".to_owned()];
        let plain = plan_decl(SweepDecl {
            rustflags: flags.clone(),
            ..SweepDecl::default()
        });
        let hash = crate::config::check::rustflags_target_key(&flags).unwrap();
        assert_eq!(
            plain.target_location(),
            TargetLocation::Isolated(IsolatedTargetKey::Rustflags(hash.clone()))
        );
        assert_eq!(
            plain.target_location().target_dir(Path::new("/target")),
            PathBuf::from(format!("/target/rustflags-{hash}"))
        );
        // ...and the two inputs compose rather than colliding: a package-mode
        // sweep carrying the same flags gets its own dir, not the plain one.
        let pkg_with_flags = plan_decl(SweepDecl {
            rustflags: flags,
            ..pkg.declaration().clone()
        });
        assert_eq!(
            pkg_with_flags.target_location(),
            TargetLocation::Isolated(IsolatedTargetKey::Unify {
                mode: CargoUnification::Package,
                rustflags_hash: Some(hash.clone()),
            })
        );
        assert_eq!(
            pkg_with_flags
                .target_location()
                .target_dir(Path::new("/target")),
            PathBuf::from(format!("/target/unify-package-{hash}"))
        );
    }

    #[test]
    fn build_shape_key_tracks_env_not_filters() {
        let plain_decl = sweep_from_check_entry(&CheckEntry {
            name: "default".into(),
            ..Default::default()
        });
        let plain = plan_decl(plain_decl.clone());
        let mut filtered = plain_decl;
        filtered.filters = LaneFilters {
            filters: vec![Filter {
                kind: FilterKind::Skip,
                pattern: "slow::".into(),
                package: None,
                origin: FilterOrigin::Entry("default".into()),
            }],
            ..Default::default()
        };
        filtered.lane_exec = LaneExec::Serial {
            threads: Threads::CargoDefault,
        };
        // Filters and thread policy don't change what cargo builds.
        assert_eq!(
            plain.build_shape_key(),
            plan_decl(filtered).build_shape_key()
        );

        // Env does: HIGH_PRECISION=1 on one sweep and not another makes two
        // otherwise-identical sweeps cache-incompatible.
        let mut declaration = plain.declaration().clone();
        declaration.env.insert("HIGH_PRECISION".into(), "1".into());
        let env_sweep = plan_decl(declaration);
        assert_ne!(plain.build_shape_key(), env_sweep.build_shape_key());
    }

    #[test]
    fn profile_rides_the_sweep_and_is_part_of_the_shape() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "timing"
profile = "release"
only = ["tape_lateness"]
curated = true

[test.profiles.timing]
sweeps = ["timing"]
"#,
        );
        let resolved = resolve(&cfg, &checks, "timing").unwrap();
        assert_eq!(resolved[0].profile, Some(SweepProfile::Release));
        assert_eq!(
            sweep_from_check_entry(&checks[0]).profile,
            Some(SweepProfile::Release)
        );

        // Unlike `curated` and `isolation`, the profile IS the build shape:
        // `cfg(debug_assertions)` decides which code exists, so a dev sweep
        // must not dedupe into a release one - it would lint one build and
        // run the other.
        let mut dev_entry = checks[0].clone();
        dev_entry.profile = Some(SweepProfile::Dev);
        let dev = sweep_from_check_entry(&dev_entry);
        assert_ne!(
            plan_decl(dev.clone()).build_shape_key(),
            plan_decl(resolved[0].clone()).build_shape_key()
        );

        // And an unset profile is its own shape too: it means "the command's
        // default", which is not the same declaration as either.
        let mut unset_entry = checks[0].clone();
        unset_entry.profile = None;
        assert_ne!(
            plan_decl(sweep_from_check_entry(&unset_entry)).build_shape_key(),
            plan_decl(dev).build_shape_key()
        );
    }

    #[test]
    fn typed_filters_name_the_block_that_actually_declared_them() {
        // The report's whole job is to name a line to delete. `extends`
        // replaces `skip` / `only` wholesale, so a filter inherited from a
        // parent is written in the PARENT - naming the resolved profile would
        // send the reader to a block that does not contain it.
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "unit"
skip = ["entry_skip"]

[test.profiles.base]
sweeps = ["unit"]
skip = ["inherited_skip"]

[test.profiles.tier2]
extends = "base"
sweeps = ["unit"]
only = ["own_only"]
"#,
        );
        let s = &resolve(&cfg, &checks, "tier2").unwrap()[0];
        let seen: Vec<(&str, &str, String)> = s
            .filters
            .filters
            .iter()
            .map(|f| (f.kind.as_str(), f.pattern.as_str(), f.origin.display()))
            .collect();
        assert_eq!(
            seen,
            vec![
                // Inherited: declared in base, not in tier2.
                ("skip", "inherited_skip", "[test.profiles.base]".into()),
                ("only", "own_only", "[test.profiles.tier2]".into()),
                // The entry's own filters AND with the profile's and keep
                // their own address.
                ("skip", "entry_skip", "[[check]] 'unit'".into()),
            ]
        );
    }

    #[test]
    fn profile_accepts_debug_as_an_alias_for_dev() {
        // `debug` is what the target subdirectory and brokkr's own --debug
        // flag call it, so a config that says `debug` must not be a parse
        // error the user has to discover at run time.
        let (checks, _) = parse_fragment(
            r#"
[[check]]
name = "fast"
profile = "debug"
"#,
        );
        assert_eq!(checks[0].profile, Some(SweepProfile::Dev));
        assert_eq!(SweepProfile::Dev.target_subdir(), "debug");
        assert!(SweepProfile::Dev.cargo_args().is_empty());
        assert_eq!(SweepProfile::Release.target_subdir(), "release");
        assert_eq!(SweepProfile::Release.cargo_args(), ["--release"]);
    }

    #[test]
    fn curated_rides_the_sweep_and_stays_out_of_the_shape() {
        let (checks, cfg) = parse_fragment(
            r#"
[[check]]
name = "sim-live"
only = ["targeted"]
curated = true

[test.profiles.sim]
sweeps = ["sim-live"]
"#,
        );
        let resolved = resolve(&cfg, &checks, "sim").unwrap();
        assert!(resolved[0].curated);
        assert!(sweep_from_check_entry(&checks[0]).curated);

        // Audit policy, not build shape: a curated and a plain sweep of an
        // otherwise-identical entry still dedupe to one clippy run - which
        // is exactly why the coverage audit must key its exemption on the
        // member sweeps, not the shape.
        let mut plain_entry = checks[0].clone();
        plain_entry.curated = false;
        let plain = sweep_from_check_entry(&plain_entry);
        assert_eq!(
            plan_decl(plain).build_shape_key(),
            plan_decl(resolved[0].clone()).build_shape_key()
        );
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
            s.features.configured_key(),
            vec!["--no-default-features", "--features", "commands"]
        );
        assert_eq!(s.build_packages, vec!["pbfhogg-cli"]);
        assert!(s.filters.libtest_args().is_empty());
        assert!(s.filters.cargo_args().is_empty());
        assert!(s.filters.name_filters().is_empty());
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
        assert_eq!(s.filters.cargo_args(), vec!["--test", "reconciliation"]);
        assert_eq!(s.filters.libtest_args(), vec!["--skip", "flaky::"]);
        assert_eq!(s.filters.name_filters(), vec!["virtual_time"]);
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
            s.filters.libtest_args(),
            vec!["--skip", "profile_skip::", "--skip", "entry_skip::"]
        );
        assert_eq!(
            s.filters.cargo_args(),
            vec!["--test", "profile_test", "--test", "entry_test"]
        );
        assert_eq!(s.filters.name_filters(), vec!["profile_only", "entry_only"]);
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
        assert_eq!(s.features.configured_key(), vec!["--features", "a,b"]);
        assert_eq!(s.build_packages, vec!["pbfhogg-cli"]);
        assert_eq!(
            s.filters.libtest_args(),
            vec!["--skip", "tier2::", "--skip", "platform::"]
        );
        assert!(s.filters.cargo_args().is_empty());
        assert!(s.filters.name_filters().is_empty());
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
        assert_eq!(s0.features.configured_key(), vec!["--features", "a"]);
        assert_eq!(
            s0.filters.libtest_args(),
            vec!["--skip", "platform::", "--skip", "serial::"]
        );
        assert_eq!(s0.filters.cargo_args(), vec!["--test", "cli_sort"]);

        let s1 = &resolved[1];
        assert_eq!(s1.label, "consumer");
        assert_eq!(
            s1.features.configured_key(),
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
        assert_eq!(r[0].filters.name_filters(), vec!["serial::"]);
        assert!(
            r[0].filters
                .libtest_args()
                .contains(&"--include-ignored".into()),
            "got: {:?}",
            r[0].filters.libtest_args()
        );
        assert_eq!(
            r[0].lane_exec,
            LaneExec::Serial {
                threads: Threads::Watchdogged
            }
        );
        assert_eq!(
            r[0].env.get("BROKKR_TEST_PLATFORM").map(String::as_str),
            Some("1")
        );
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
        assert_eq!(
            r[0].env.get("HIGH_PRECISION").map(String::as_str),
            Some("1")
        );
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
        assert_eq!(
            r[0].lane_exec,
            LaneExec::Serial {
                threads: Threads::Cargo(NonZeroU32::new(8).unwrap())
            }
        );
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
        assert_eq!(r[0].filters.cargo_args(), vec!["--test", "cli_x"]);
        assert!(
            r[0].filters
                .libtest_args()
                .contains(&"--include-ignored".into())
        );
        assert_eq!(
            r[0].filters
                .libtest_args()
                .iter()
                .filter(|s| s.as_str() == "a::")
                .count(),
            1
        );
    }

    #[test]
    fn feature_tokens_split_on_commas_and_whitespace() {
        let entry = CheckEntry {
            name: "x".into(),
            features: vec!["a,b c".into(), " ,d/e ".into()],
            ..Default::default()
        };
        let cfg = FeatureConfig::from_entry(&entry);
        assert_eq!(cfg.tokens, vec!["a", "b", "c", "d/e"]);
        assert!(!cfg.invocation_explicit);
        assert_eq!(cfg.argv_with(&[]), Vec::<String>::new());
        assert_eq!(cfg.configured_key(), vec!["--features", "a,b,c,d/e"]);
        let adhoc = FeatureConfig::explicit(&["x".into()], true, false);
        assert!(adhoc.invocation_explicit);
        assert_eq!(
            adhoc.configured_key(),
            vec!["--no-default-features", "--features", "x"]
        );
    }

    #[test]
    fn libtest_argv_concats_args_and_name_filters() {
        let s = SweepDecl {
            label: "x".into(),
            filters: LaneFilters {
                include_ignored: true,
                filters: vec![Filter {
                    kind: FilterKind::Only,
                    pattern: "tier2::".into(),
                    package: None,
                    origin: FilterOrigin::Entry("x".into()),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(s.libtest_argv(), vec!["--include-ignored", "tier2::"]);
    }
}
