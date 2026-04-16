//! Unified measurement types for the CLI redesign.
//!
//! Replaces the separate `BenchRequest` / `HotpathRequest` / `ProfileRequest`
//! structs with a single `MeasureRequest` carrying a `MeasureMode`.  Also
//! defines `CommandContext` — the resolved runtime context that a command's
//! arg-builder needs to produce its argument vector.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::DevError;
use crate::project::Project;

// ---------------------------------------------------------------------------
// MeasureMode
// ---------------------------------------------------------------------------

/// How to measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasureMode {
    /// Run once, print timing. Acquires lockfile but no DB storage. Default.
    Run,
    /// Full benchmark: lockfile, N runs (best-of-N), DB storage.
    Bench { runs: usize },
    /// Function-level timing via `--features hotpath` + `run_hotpath_capture()`.
    Hotpath { runs: usize },
    /// Per-function allocation tracking via `--features hotpath-alloc` +
    /// `run_hotpath_capture()`.
    Alloc { runs: usize },
}

impl MeasureMode {
    /// Number of runs for this mode.
    pub fn runs(self) -> usize {
        match self {
            Self::Run => 1,
            Self::Bench { runs } | Self::Hotpath { runs } | Self::Alloc { runs } => runs,
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
    /// Which project we are running in.
    pub project: Project,
    /// Root of the project (where `brokkr.toml` lives).
    pub project_root: &'a Path,
    /// Optional alternate build root (worktree for retroactive benchmarking).
    pub build_root: Option<&'a Path>,
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
    /// The literal `brokkr <...>` invocation (std::env::args joined).
    /// Stored on each result row so queries can grep the command the user
    /// actually typed, even when brokkr translates it into something
    /// different before handing off to the tool subprocess.
    pub brokkr_args: &'a str,
    /// Skip the OOM memory check.
    pub no_mem_check: bool,
    /// Wait for the lock instead of failing immediately.
    pub wait: bool,
    /// Dry-run: resolve paths and build the arg vector without building the
    /// binary or running it. See `ModeArgs::dry_run`.
    pub dry_run: bool,
    /// Kill the child when this marker is emitted via the sidecar FIFO.
    pub stop_marker: Option<&'a str>,
}

impl MeasureRequest<'_> {
    /// Number of measurement runs for this request.
    pub fn runs(&self) -> usize {
        self.mode.runs()
    }

    /// Whether we are in allocation profiling mode.
    pub fn is_alloc(&self) -> bool {
        matches!(self.mode, MeasureMode::Alloc { .. })
    }

    /// Variant string recorded in the DB for this measurement mode.
    ///
    /// Returns `None` for `Run` mode (which doesn't write to the DB at
    /// all). The three DB-writing modes map to `"bench"`, `"hotpath"`,
    /// `"alloc"`.
    pub fn variant_mode(&self) -> Option<&'static str> {
        match self.mode {
            MeasureMode::Run => None,
            MeasureMode::Bench { .. } => Some("bench"),
            MeasureMode::Hotpath { .. } => Some("hotpath"),
            MeasureMode::Alloc { .. } => Some("alloc"),
        }
    }

    /// Build the full feature list for hotpath/alloc builds.
    ///
    /// Prepends the hotpath (or hotpath-alloc) feature to the user/host features.
    pub fn hotpath_features(&self) -> Vec<&str> {
        let feature = crate::harness::hotpath_feature(self.is_alloc());
        let mut all: Vec<&str> = vec![feature];
        all.extend(self.features.iter().map(String::as_str));
        all
    }

    /// Feature refs as `&str` slices (for passing to build functions).
    pub fn feat_refs(&self) -> Vec<&str> {
        self.features.iter().map(String::as_str).collect()
    }

    /// The effective build root (worktree if set, otherwise project root).
    pub fn effective_build_root(&self) -> &Path {
        self.build_root.unwrap_or(self.project_root)
    }
}

// ---------------------------------------------------------------------------
// CommandContext
// ---------------------------------------------------------------------------

/// Everything a command's arg-builder needs to construct an argument vector.
///
/// Populated by the dispatch layer after resolving dataset paths, scratch
/// directories, and command-specific parameters.  Passed to each command's
/// `build_args` method.
pub struct CommandContext {
    /// Path to the compiled binary.
    pub binary: PathBuf,
    /// Resolved PBF file path.
    pub pbf_path: PathBuf,
    /// Resolved OSC file path (when the command needs one).
    ///
    /// For commands that accept multiple OSCs (e.g. `merge-changes --osc-range`),
    /// this holds the first entry for back-compat; use `osc_paths` for the full list.
    pub osc_path: Option<PathBuf>,
    /// Resolved OSC file paths in ascending seq order.
    ///
    /// Single-OSC commands populate this with a one-element vec; multi-OSC commands
    /// (merge-changes with a range) populate it with the full expanded range.
    pub osc_paths: Vec<PathBuf>,
    /// Resolved B-side PBF path for any diff-style operation. Populated by:
    /// - `Diff` / `DiffOsc` from `ensure_merged_pbf` (apply-changes output).
    /// - `DiffSnapshots` from `resolve_snapshot_pbf_path` for the `--to` side.
    pub pbf_b_path: Option<PathBuf>,
    /// Scratch directory for output files.
    pub scratch_dir: PathBuf,
    /// Dataset name (e.g. `"denmark"`).
    pub dataset: String,
    /// Bounding box string (e.g. `"12.4,55.6,12.7,55.8"`).
    pub bbox: Option<String>,
    /// Command-specific parameters (e.g. `"index_type" -> "external"`).
    pub params: HashMap<String, String>,
}

impl CommandContext {
    /// Get the PBF path as a UTF-8 string, or error.
    pub fn pbf_str(&self) -> Result<&str, DevError> {
        self.pbf_path
            .to_str()
            .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))
    }

    /// Get the OSC path as a UTF-8 string, or error if not set.
    pub fn osc_str(&self) -> Result<&str, DevError> {
        self.osc_path
            .as_ref()
            .ok_or_else(|| DevError::Config("OSC path is required but not set".into()))?
            .to_str()
            .ok_or_else(|| DevError::Config("OSC path is not valid UTF-8".into()))
    }

    /// Get all OSC paths as UTF-8 strings, or error if none set.
    pub fn osc_strs(&self) -> Result<Vec<&str>, DevError> {
        if self.osc_paths.is_empty() {
            return Err(DevError::Config("OSC paths are required but not set".into()));
        }
        self.osc_paths
            .iter()
            .map(|p| {
                p.to_str()
                    .ok_or_else(|| DevError::Config("OSC path is not valid UTF-8".into()))
            })
            .collect()
    }

    /// Get the B-side PBF path as a UTF-8 string, or error if not set.
    pub fn pbf_b_str(&self) -> Result<&str, DevError> {
        self.pbf_b_path
            .as_ref()
            .ok_or_else(|| DevError::Config("B-side PBF path is required but not set".into()))?
            .to_str()
            .ok_or_else(|| DevError::Config("B-side PBF path is not valid UTF-8".into()))
    }

    /// Get the binary path as a UTF-8 string, or error.
    pub fn binary_str(&self) -> Result<&str, DevError> {
        self.binary
            .to_str()
            .ok_or_else(|| DevError::Config("binary path is not valid UTF-8".into()))
    }

    /// Look up a command-specific parameter by key.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }

    /// Build a scratch output file path with the given name and extension.
    pub fn scratch_output(&self, name: &str, ext: &str) -> PathBuf {
        self.scratch_dir.join(format!("{name}.{ext}"))
    }

    /// PBF file basename (e.g. `"denmark-with-indexdata.osm.pbf"`).
    pub fn pbf_basename(&self) -> String {
        self.pbf_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned()
    }
}
