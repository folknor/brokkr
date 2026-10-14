//! Unified measurement types for the CLI redesign.
//!
//! Replaces the separate `BenchRequest` / `HotpathRequest` / `ProfileRequest`
//! structs with a single `MeasureRequest` carrying a `MeasureMode`.

use std::num::NonZeroUsize;

use crate::config;
use crate::project::{BuildRoot, Project, ProjectRoot};

// ---------------------------------------------------------------------------
// MeasureMode
// ---------------------------------------------------------------------------

/// How to measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasureMode {
    /// Run once, print timing. Acquires lockfile but no DB storage. Default.
    Run,
    /// Full benchmark: lockfile, N runs (best-of-N), DB storage.
    Bench { runs: NonZeroUsize },
    /// Function-level timing via `--features hotpath` + `BenchHarness::capture_hotpath()`.
    Hotpath { runs: NonZeroUsize },
    /// Per-function allocation tracking via `--features hotpath-alloc` +
    /// `BenchHarness::capture_hotpath()`.
    Alloc { runs: NonZeroUsize },
}

/// A measurement mode that is stored on a results row.
///
/// Run mode has no database row, so it deliberately has no `RecordedMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedMode {
    Bench,
    Hotpath,
    Alloc,
}

impl RecordedMode {
    /// Stable label stored in the results database.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bench => "bench",
            Self::Hotpath => "hotpath",
            Self::Alloc => "alloc",
        }
    }
}

/// The profiling build selected by a measurement request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfiledMode {
    Hotpath,
    Alloc,
}

impl ProfiledMode {
    /// Cargo feature that enables this instrumentation.
    pub const fn feature(self) -> &'static str {
        match self {
            Self::Hotpath => "hotpath",
            Self::Alloc => "hotpath-alloc",
        }
    }

    /// Optional caveat printed before an allocation-profiled run.
    pub const fn note(self) -> Option<&'static str> {
        match self {
            Self::Hotpath => None,
            Self::Alloc => Some("NOTE: alloc profiling - wall-clock times are not meaningful"),
        }
    }

    /// Print [`Self::note`], when there is one. Every profiled entry point
    /// calls this, so the caveat's wording lives in one place.
    pub fn print_note(self) {
        if let Some(note) = self.note() {
            crate::output::hotpath_msg(note);
        }
    }
}

impl MeasureRequest<'_> {
    /// The cargo features a build for this request carries: the profiled
    /// mode's instrumentation feature first (when there is one), then the
    /// user/host features.
    pub fn build_features(&self) -> Vec<&str> {
        let mut all: Vec<&str> = self
            .mode
            .profiled()
            .map(ProfiledMode::feature)
            .into_iter()
            .collect();
        for feature in self.features.iter().map(String::as_str) {
            if !all.contains(&feature) {
                all.push(feature);
            }
        }
        all
    }
}

impl MeasureMode {
    /// Number of runs for this mode. Zero cannot be represented.
    pub const fn runs(self) -> NonZeroUsize {
        match self {
            Self::Run => NonZeroUsize::MIN,
            Self::Bench { runs } | Self::Hotpath { runs } | Self::Alloc { runs } => runs,
        }
    }

    /// The mode to persist, if this request records results.
    pub const fn recorded(self) -> Option<RecordedMode> {
        match self {
            Self::Run => None,
            Self::Bench { .. } => Some(RecordedMode::Bench),
            Self::Hotpath { .. } => Some(RecordedMode::Hotpath),
            Self::Alloc { .. } => Some(RecordedMode::Alloc),
        }
    }

    /// Profiling instrumentation selected by this mode, if any.
    pub const fn profiled(self) -> Option<ProfiledMode> {
        match self {
            Self::Hotpath { .. } => Some(ProfiledMode::Hotpath),
            Self::Alloc { .. } => Some(ProfiledMode::Alloc),
            Self::Run | Self::Bench { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// MeasureRequest
// ---------------------------------------------------------------------------

/// Unified request struct replacing `BenchRequest` / `HotpathRequest` /
/// `ProfileRequest`.
///
/// Carries everything needed to set up a measurement: project context, dataset
/// selection, build options, and the measurement mode.
pub struct MeasureRequest<'a> {
    /// Parsed `brokkr.toml` configuration.
    pub dev_config: &'a config::DevConfig,
    /// The section parsed for `project` (dellingr's workloads, mogwai's
    /// targets, piners' paths).
    pub project_config: &'a config::project_config::ProjectConfig,
    /// Which project we are running in.
    pub project: Project,
    /// Root of the project (where `brokkr.toml` lives): state, stores, data.
    pub project_root: &'a ProjectRoot,
    /// The code tree being built: the live checkout, or the `--commit`
    /// worktree cut from it.
    pub build_root: &'a BuildRoot,
    /// Dataset name from `--dataset` (e.g. `"denmark"`).
    pub dataset: &'a str,
    /// PBF variant from `--variant` (e.g. `"indexed"`, `"raw"`).
    pub variant: &'a str,
    /// Extra cargo features from CLI `--features` and host config.
    pub features: &'a [String],
    /// Allow benchmarking on a dirty git tree.
    pub force: bool,
    /// How to measure.
    pub mode: MeasureMode,
    /// The brokkr invocation: the exact argv and its arm key
    /// (`crate::run_invocation`). Stored on each result row so queries can
    /// grep the command the user actually typed, even when brokkr translates
    /// it into something different before handing off to the tool
    /// subprocess, and so `--compare` pairs rows without re-parsing it.
    pub invocation: &'a crate::db::RunInvocation,
    /// Dry-run: resolve paths and build the arg vector without building the
    /// binary or running it. See `ModeArgs::dry_run`.
    pub dry_run: bool,
    /// Kill the child when this marker is emitted via the sidecar FIFO.
    pub stop_marker: Option<&'a str>,
}

impl MeasureRequest<'_> {
    /// Number of measurement runs for this request.
    pub fn runs(&self) -> usize {
        self.mode.runs().get()
    }

    /// Feature refs as `&str` slices (for passing to build functions).
    pub fn feat_refs(&self) -> Vec<&str> {
        self.features.iter().map(String::as_str).collect()
    }
}

/// Shared execution of standalone command plans.
pub(crate) mod runner;
