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

use crate::config::{
    CheckEntry, Isolation, ProfileDef, QualifiedSkip, SkipSpec, SweepProfile, TestConfig,
};
use crate::config::ParallelBinaries;
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
    /// From the `[[check]]` entry's `parallel.budget`: run this sweep's test
    /// binaries concurrently under a budget of in-flight tests, rather than
    /// letting cargo run them one after another. Execution policy only -
    /// never part of the build shape, since it changes neither what is
    /// compiled nor which tests are selected. Mutually exclusive with
    /// `process_isolation` (enforced at resolve time): one runs every test in
    /// its own process, the other runs many binaries' tests at once, and a
    /// sweep cannot mean both.
    pub parallel_budget: Option<u32>,
    /// Run each test in its own `cargo test -- --exact` process (the
    /// profile's `isolation = "process"`). Execution policy only - never
    /// part of the build shape.
    pub process_isolation: bool,
    /// Package-qualified skips, filtered out of the enumerated set
    /// (never expressed as cargo selection). Non-empty only on
    /// process-isolated sweeps (enforced at resolve time). A filter,
    /// never part of the build shape.
    pub qualified_skips: Vec<QualifiedSkip>,
    /// From the `[[check]]` entry's `curated`: the entry runs a hand-picked
    /// subset, and its shape's non-run pairs are exempt from the coverage
    /// universe. Audit policy only - never part of the build shape, and the
    /// audit exempts a shape only when *every* sweep producing it is curated.
    pub curated: bool,
    /// Every `skip` / `only` filter this sweep carries, each still knowing
    /// where it was written. The executable forms above are flattened past
    /// recovery - profile `skip` and entry `skip` both become `--skip X` pairs
    /// in `libtest_args`, and both `only` lists concatenate into
    /// `name_filters` - so a dead-filter report built from those could only
    /// name the sweep, not the line to delete. Audit provenance only: never
    /// part of the build shape, never read by an execution path.
    pub declared_filters: Vec<DeclaredFilter>,
    /// The `[[check]]` entry's `profile`: the cargo profile this sweep
    /// compiles and runs under, or `None` for the command's default (dev
    /// under `brokkr check`; the CLI/`[test] debug` answer under `brokkr
    /// test`). Part of the build shape - see [`ResolvedSweep::build_shape_key`].
    pub profile: Option<SweepProfile>,
}

/// Which half of the filter surface a [`DeclaredFilter`] is, because the two
/// are dead for different reasons and are checked against different sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// One `skip` / `only` filter with the config location it came from - the
/// input to the coverage phase's alive-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredFilter {
    pub kind: FilterKind,
    /// The test-name substring, as written.
    pub pattern: String,
    /// Set on a package-qualified skip; the pattern then applies within this
    /// package only, exactly as a `[[quarantine]]` entry's `package` scopes it.
    pub package: Option<String>,
    /// Where it was written: `[test.profiles.tier2]` or `[[check]] 'serial'`.
    /// Carried as rendered text because the report's whole job is to name a
    /// line, and the two sources have no common address type.
    pub origin: String,
}

impl DeclaredFilter {
    /// Does this filter match `test` in `package`? Substring semantics, the
    /// same libtest applies to both `--skip` and a positional filter, with the
    /// package scoping that only a qualified skip carries.
    pub fn matches(&self, package: &str, test: &str) -> bool {
        self.package.as_deref().is_none_or(|p| p == package) && test.contains(&self.pattern)
    }

    /// How the dead-filter report names it.
    pub fn label(&self) -> String {
        match &self.package {
            Some(pkg) => format!("{} \"{}\" (package {pkg})", self.kind.as_str(), self.pattern),
            None => format!("{} \"{}\"", self.kind.as_str(), self.pattern),
        }
    }
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

    /// The sweep's *build shape*: everything that decides what cargo
    /// compiles, and nothing that only decides which tests run. Two lanes
    /// referencing the same `[[check]]` entry produce equal keys, so
    /// clippy (and `brokkr test`, which drops filters) dedupe on this
    /// while the test phase keeps both entries. `env` is in the key:
    /// `HIGH_PRECISION=1` on one sweep and not another makes two
    /// otherwise-identical sweeps cache-incompatible. `test_exclude_packages`
    /// is deliberately out - it narrows the test invocation only.
    ///
    /// `profile` is in, because `cfg(debug_assertions)` decides which code
    /// exists: a dev and a release sweep of the same features compile
    /// different source and present different lint surfaces, so deduping
    /// either into the other would lint one build and run the other.
    pub fn build_shape_key(&self) -> BuildShapeKey {
        (
            self.packages.clone(),
            self.cargo_feature_args.clone(),
            self.rustflags.clone(),
            self.env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            self.build_packages.clone(),
            self.profile,
        )
    }
}

/// See [`ResolvedSweep::build_shape_key`].
pub type BuildShapeKey = (
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<(String, String)>,
    Vec<String>,
    Option<SweepProfile>,
);

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
    let declared_filters = entry_filters(entry);
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
        parallel_budget: entry.parallel.map(ParallelBinaries::resolved_budget),
        process_isolation: false,
        qualified_skips: Vec::new(),
        declared_filters,
        curated: entry.curated,
        profile: entry.profile,
    }
}

/// The `[[check]]` entry's own filters, provenance attached.
fn entry_filters(entry: &CheckEntry) -> Vec<DeclaredFilter> {
    let origin = format!("[[check]] '{}'", entry.name);
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

/// Resolve one non-`lanes` profile (the pre-lanes `resolve` body).
fn resolve_single(
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
        out.push(build_resolved_sweep(entry, &merged, name));
    }

    // Package-qualified skips are filtered out of the enumerated set, and
    // the enumerated set only exists on the process-isolated path - a
    // shared-process libtest run has no per-package attribution to filter.
    if out
        .iter()
        .any(|s| !s.qualified_skips.is_empty() && !s.process_isolation)
    {
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
        let (key, def) = profiles.get_key_value(&cur).ok_or_else(|| {
            DevError::Config(format!("[test.profiles.{cur}] is not defined"))
        })?;
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
/// Split out of [`build_resolved_sweep`] so the ad-hoc CLI-features path can
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
    libtest_args: Vec<String>,
    cargo_test_filters: Vec<String>,
    name_filters: Vec<String>,
    env: BTreeMap<String, String>,
    test_threads: Option<u32>,
    process_isolation: bool,
    qualified_skips: Vec<QualifiedSkip>,
    declared_filters: Vec<DeclaredFilter>,
}

impl RunShaping {
    /// True when the profile shapes nothing - a `lanes` profile, or one
    /// declaring no filters. Lets the caller say "no filters applied"
    /// rather than name a profile that changed nothing.
    pub fn is_empty(&self) -> bool {
        self.libtest_args.is_empty()
            && self.cargo_test_filters.is_empty()
            && self.name_filters.is_empty()
            && self.env.is_empty()
            && self.test_threads.is_none()
            && !self.process_isolation
            && self.qualified_skips.is_empty()
    }

    /// Overlay onto an ad-hoc sweep. Assignment, not merge: an ad-hoc sweep
    /// carries no `[[check]]` entry, so there is nothing to AND with.
    pub fn apply(self, sweep: &mut ResolvedSweep) {
        sweep.libtest_args = self.libtest_args;
        sweep.cargo_test_filters = self.cargo_test_filters;
        sweep.name_filters = self.name_filters;
        sweep.env = self.env;
        sweep.test_threads = self.test_threads;
        sweep.process_isolation = self.process_isolation;
        sweep.qualified_skips = self.qualified_skips;
        sweep.declared_filters = self.declared_filters;
    }
}

/// The run shaping `name` contributes, for the ad-hoc CLI-features path.
///
/// A `lanes` profile returns an empty shaping: load-time validation
/// guarantees lanes carry no run-shaping fields of their own, and there is
/// no defensible way to pick one lane's filters for a single ad-hoc run.
pub fn run_shaping(cfg: &TestConfig, name: &str) -> Result<RunShaping, DevError> {
    if cfg.profiles.get(name).is_some_and(|d| d.lanes.is_some()) {
        return Ok(RunShaping::default());
    }
    Ok(shaping_from(
        &resolve_profile_chain(&cfg.profiles, name)?,
        name,
    ))
}

/// `profile_name` is carried for provenance alone: the merged profile is the
/// product of an `extends` chain and has no name of its own, but a dead-filter
/// report has to name a block the reader can open.
fn shaping_from(profile: &ResolvedProfile, profile_name: &str) -> RunShaping {
    // `test_threads` is NOT pushed into `libtest_args` here. It is carried raw
    // on the sweep so each consumer applies its own policy: the check test
    // phase turns it into `--test-threads=N` (or a parallel run), while
    // `brokkr test` ignores it and forces `--test-threads=1`.
    let mut libtest_args: Vec<String> = Vec::new();
    if profile.include_ignored {
        libtest_args.push("--include-ignored".into());
    }
    // Name entries become libtest `--skip` flags; qualified entries are
    // carried separately and filtered out of the enumerated set instead.
    let mut qualified_skips: Vec<QualifiedSkip> = Vec::new();
    for spec in &profile.skip {
        match spec {
            SkipSpec::Name(s) => {
                libtest_args.push("--skip".into());
                libtest_args.push(s.clone());
            }
            SkipSpec::Qualified(q) => qualified_skips.push(q.clone()),
        }
    }

    let mut cargo_test_filters: Vec<String> = Vec::new();
    for t in &profile.tests {
        cargo_test_filters.push("--test".into());
        cargo_test_filters.push(t.clone());
    }

    // Provenance is per list, not per profile: `extends` replaces `only` and
    // `skip` wholesale, so each names the def that actually declared it.
    let skip_at = at_profile(profile.skip_from.as_deref(), profile_name);
    let only_at = at_profile(profile.only_from.as_deref(), profile_name);
    let mut declared_filters: Vec<DeclaredFilter> = Vec::new();
    for spec in &profile.skip {
        let (pattern, package) = match spec {
            SkipSpec::Name(s) => (s.clone(), None),
            SkipSpec::Qualified(q) => (q.pattern.clone(), Some(q.package.clone())),
        };
        declared_filters.push(DeclaredFilter {
            kind: FilterKind::Skip,
            pattern,
            package,
            origin: skip_at.clone(),
        });
    }
    declared_filters.extend(profile.only.iter().map(|o| DeclaredFilter {
        kind: FilterKind::Only,
        pattern: o.clone(),
        package: None,
        origin: only_at.clone(),
    }));

    RunShaping {
        libtest_args,
        cargo_test_filters,
        declared_filters,
        name_filters: profile.only.clone(),
        env: profile.env.clone(),
        test_threads: profile.test_threads,
        process_isolation: profile.isolation == Some(Isolation::Process),
        qualified_skips,
    }
}

/// Render a filter's origin block, falling back to the resolved profile when
/// the chain recorded no declaring def (the list is empty in that case, so the
/// fallback never actually labels a filter).
fn at_profile(declared_in: Option<&str>, resolved: &str) -> String {
    format!("[test.profiles.{}]", declared_in.unwrap_or(resolved))
}

fn build_resolved_sweep(
    entry: &CheckEntry,
    profile: &ResolvedProfile,
    profile_name: &str,
) -> ResolvedSweep {
    let shaping = shaping_from(profile, profile_name);
    // Profile `skip` then the entry's own `skip` - they AND (both apply), never
    // replace, so a sweep can pin its own exclusions on top of the profile's.
    let mut libtest_args = shaping.libtest_args;
    let qualified_skips = shaping.qualified_skips;
    for s in &entry.skip {
        libtest_args.push("--skip".into());
        libtest_args.push(s.clone());
    }

    let mut cargo_test_filters = shaping.cargo_test_filters;
    for t in &entry.tests {
        cargo_test_filters.push("--test".into());
        cargo_test_filters.push(t.clone());
    }

    // Profile `only` (positional substring filters) then the entry's own.
    let mut name_filters = shaping.name_filters;
    name_filters.extend(entry.only.iter().cloned());

    // The profile's filters and the entry's own AND together, so the audit
    // sees both lists - each still pointing at its own block.
    let mut declared_filters = shaping.declared_filters;
    declared_filters.extend(entry_filters(entry));

    // Profile env is the base; the entry's own env overlays it so a
    // sweep-specific var wins on a key collision.
    let mut env = shaping.env;
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
        parallel_budget: entry.parallel.map(ParallelBinaries::resolved_budget),
        process_isolation: profile.isolation == Some(Isolation::Process),
        qualified_skips,
        declared_filters,
        curated: entry.curated,
        profile: entry.profile,
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
        assert_eq!(sweeps[0].test_threads, Some(0));
        assert_eq!(sweeps[1].libtest_argv(), vec!["serial::"]);
        assert_eq!(sweeps[1].test_threads, Some(1));
        // ...while the build shape is identical, which is exactly what
        // clippy (and `brokkr test`) dedupe on.
        assert_eq!(sweeps[0].build_shape_key(), sweeps[1].build_shape_key());
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
        assert!(sweeps[0].process_isolation);

        // Execution policy, not build shape: an isolated and a plain sweep
        // of the same entry still dedupe to one clippy run.
        let plain = sweep_from_check_entry(&checks[0]);
        assert_eq!(plain.build_shape_key(), sweeps[0].build_shape_key());
    }

    #[test]
    fn build_shape_key_tracks_env_not_filters() {
        let plain = sweep_from_check_entry(&CheckEntry {
            name: "default".into(),
            ..Default::default()
        });
        let mut filtered = plain.clone();
        filtered.libtest_args = vec!["--skip".into(), "slow::".into()];
        filtered.test_threads = Some(0);
        // Filters and thread policy don't change what cargo builds.
        assert_eq!(plain.build_shape_key(), filtered.build_shape_key());

        // Env does: HIGH_PRECISION=1 on one sweep and not another makes two
        // otherwise-identical sweeps cache-incompatible.
        let mut env_sweep = plain.clone();
        env_sweep
            .env
            .insert("HIGH_PRECISION".into(), "1".into());
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
        assert_ne!(dev.build_shape_key(), resolved[0].build_shape_key());

        // And an unset profile is its own shape too: it means "the command's
        // default", which is not the same declaration as either.
        let mut unset_entry = checks[0].clone();
        unset_entry.profile = None;
        assert_ne!(
            sweep_from_check_entry(&unset_entry).build_shape_key(),
            dev.build_shape_key()
        );
    }

    #[test]
    fn declared_filters_name_the_block_that_actually_declared_them() {
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
        let seen: Vec<(&str, &str, &str)> = s
            .declared_filters
            .iter()
            .map(|f| (f.kind.as_str(), f.pattern.as_str(), f.origin.as_str()))
            .collect();
        assert_eq!(
            seen,
            vec![
                // Inherited: declared in base, not in tier2.
                ("skip", "inherited_skip", "[test.profiles.base]"),
                ("only", "own_only", "[test.profiles.tier2]"),
                // The entry's own filters AND with the profile's and keep
                // their own address.
                ("skip", "entry_skip", "[[check]] 'unit'"),
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
        assert_eq!(plain.build_shape_key(), resolved[0].build_shape_key());
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
