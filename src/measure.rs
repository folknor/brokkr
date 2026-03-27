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
    /// Run once, print timing. No lockfile, no DB. Default.
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
    /// The cargo feature name required for this mode, if any.
    pub fn cargo_feature(self) -> Option<&'static str> {
        match self {
            Self::Run | Self::Bench { .. } => None,
            Self::Hotpath { .. } => Some("hotpath"),
            Self::Alloc { .. } => Some("hotpath-alloc"),
        }
    }

    /// Whether this mode uses hotpath-style capture (JSON report file).
    pub fn is_hotpath_capture(self) -> bool {
        matches!(self, Self::Hotpath { .. } | Self::Alloc { .. })
    }

    /// Whether this mode tracks allocations (affects OOM risk assessment).
    pub fn is_alloc(self) -> bool {
        matches!(self, Self::Alloc { .. })
    }

    /// Number of runs for this mode.
    pub fn runs(self) -> usize {
        match self {
            Self::Run => 1,
            Self::Bench { runs } | Self::Hotpath { runs } | Self::Alloc { runs } => runs,
        }
    }

    /// The variant suffix appended to result variant names.
    pub fn variant_suffix(self) -> &'static str {
        match self {
            Self::Alloc { .. } => "/alloc",
            _ => "",
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
    /// Number of measurement runs.
    pub runs: usize,
    /// Extra cargo features from CLI `--features` and host config.
    pub features: &'a [String],
    /// Allow benchmarking on a dirty git tree.
    pub force: bool,
    /// How to measure.
    pub mode: MeasureMode,
    /// Skip the OOM memory check.
    pub no_mem_check: bool,
}

impl<'a> MeasureRequest<'a> {
    /// Whether allocation tracking is active (derived from mode).
    pub fn is_alloc(&self) -> bool {
        self.mode.is_alloc()
    }

    /// The effective build root: worktree path if set, otherwise project root.
    pub fn effective_build_root(&self) -> &'a Path {
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
    pub osc_path: Option<PathBuf>,
    /// Resolved merged PBF path (for diff/diff-osc commands).
    pub merged_pbf_path: Option<PathBuf>,
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

    /// Get the merged PBF path as a UTF-8 string, or error if not set.
    pub fn merged_pbf_str(&self) -> Result<&str, DevError> {
        self.merged_pbf_path
            .as_ref()
            .ok_or_else(|| DevError::Config("merged PBF path is required but not set".into()))?
            .to_str()
            .ok_or_else(|| DevError::Config("merged PBF path is not valid UTF-8".into()))
    }

    /// Get the scratch directory as a UTF-8 string, or error.
    pub fn scratch_str(&self) -> Result<&str, DevError> {
        self.scratch_dir
            .to_str()
            .ok_or_else(|| DevError::Config("scratch dir path is not valid UTF-8".into()))
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
