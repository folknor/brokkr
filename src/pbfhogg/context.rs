//! Pbfhogg command parameters, resolved inputs, and runtime observations.

use std::path::{Path, PathBuf};

use crate::config::OscRange;
use crate::datasets::{Bbox, ElementIdList};
use crate::error::DevError;
use crate::pbfhogg::Compression;

/// Pbfhogg I/O switches and their cargo/argv representation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IoMode {
    pub direct_io: bool,
    pub io_uring: bool,
}

impl IoMode {
    /// Cargo features required to compile this I/O mode.
    pub fn features(self) -> Vec<&'static str> {
        let mut features = Vec::new();
        if self.direct_io {
            features.push("linux-direct-io");
        }
        if self.io_uring {
            features.push("linux-io-uring");
        }
        features
    }

    /// Command-line switches for this I/O mode.
    pub fn args(self) -> Vec<String> {
        let mut args = Vec::new();
        if self.direct_io {
            args.push("--direct-io".into());
        }
        if self.io_uring {
            args.push("--io-uring".into());
        }
        args
    }
}

/// Typed command-specific parameters populated from brokkr CLI arguments.
#[derive(Debug, Default, Clone)]
pub struct CommandParams {
    pub io: IoMode,
    pub compression: Option<Compression>,
    pub bbox: Option<Bbox>,
    pub index_type: Option<String>,
    /// Forward the pbfhogg injected-prepass wire-format option.
    pub inject_prepass: bool,
    /// Forward pbfhogg's `--force` option for add-locations-to-ways.
    pub force_altw: bool,
    pub locations_on_ways: bool,
    pub snapshot: Option<crate::datasets::resolve::SnapshotRef>,
    pub from_snapshot: Option<crate::datasets::resolve::SnapshotRef>,
    pub to_snapshot: Option<crate::datasets::resolve::SnapshotRef>,
    pub osc_range: Option<OscRange>,
    pub keep_cache: bool,
    /// `0` asks pbfhogg to resolve the worker count from available CPUs.
    pub jobs: Option<usize>,
    pub as_snapshot: Option<String>,
    pub replace_snapshot: bool,
    /// The dataset's configured getid/getparents bench IDs; `None` uses the
    /// built-in lists.
    pub selection: BenchSelection,
}

/// The element IDs a dataset configures for the getid and getparents
/// benches (`getid_ids`, `getparents_ids`). An absent list means the
/// built-in one, so a dataset without the keys measures what it always has.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BenchSelection {
    pub getid: Option<ElementIdList>,
    pub getparents: Option<ElementIdList>,
}

impl BenchSelection {
    /// The selection a configured dataset declares.
    pub fn for_dataset(dataset: Option<&crate::config::Dataset>) -> Self {
        dataset.map_or_else(Self::default, |dataset| Self {
            getid: dataset.getid_ids.clone(),
            getparents: dataset.getparents_ids.clone(),
        })
    }
}

/// Origin of a two-PBF command input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwoPbfOrigin {
    /// The second file is the result of applying the selected OSC to the first.
    Merged,
    /// Both files were resolved independently from named snapshots.
    Snapshot,
}

/// Fully resolved input shape for one pbfhogg command.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedInputs {
    Pbf {
        pbf: PathBuf,
    },
    PbfAndOsc {
        pbf: PathBuf,
        osc: PathBuf,
    },
    PbfAndBbox {
        pbf: PathBuf,
        bbox: Bbox,
    },
    OscOnly {
        oscs: Vec<PathBuf>,
    },
    TwoPbfs {
        a: PathBuf,
        b: PathBuf,
        origin: TwoPbfOrigin,
    },
}

impl ResolvedInputs {
    pub fn primary_pbf(&self) -> Option<&Path> {
        match self {
            Self::Pbf { pbf } | Self::PbfAndOsc { pbf, .. } | Self::PbfAndBbox { pbf, .. } => {
                Some(pbf)
            }
            Self::TwoPbfs { a, .. } => Some(a),
            Self::OscOnly { .. } => None,
        }
    }

    pub fn second_pbf(&self) -> Option<&Path> {
        match self {
            Self::TwoPbfs { b, .. } => Some(b),
            _ => None,
        }
    }

    pub fn osc_paths(&self) -> &[PathBuf] {
        match self {
            Self::PbfAndOsc { osc, .. } => std::slice::from_ref(osc),
            Self::OscOnly { oscs } => oscs,
            _ => &[],
        }
    }

    pub fn bbox(&self) -> Option<&Bbox> {
        match self {
            Self::PbfAndBbox { bbox, .. } => Some(bbox),
            _ => None,
        }
    }
}

/// Cache observation recorded for diff runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergedCacheState {
    Hit { age_secs: u64 },
    Miss,
}

/// Resolved information about the second side of a snapshot diff.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotSide {
    pub file_name: Option<String>,
    pub size_mb: Option<f64>,
}

/// Runtime facts recorded alongside a measured command result.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CommandObservations {
    pub merged_cache: Option<MergedCacheState>,
    pub snapshot_to: Option<SnapshotSide>,
}

/// Command execution context after all input paths have been resolved.
#[derive(Debug, Clone)]
pub struct CommandContext {
    pub inputs: ResolvedInputs,
    pub scratch_dir: PathBuf,
    pub dataset: String,
    pub params: CommandParams,
    pub observations: CommandObservations,
}

impl CommandContext {
    pub fn pbf_str(&self) -> Result<&str, DevError> {
        let path = self
            .inputs
            .primary_pbf()
            .ok_or_else(|| DevError::Internal("pbfhogg command input has no primary PBF".into()))?;
        crate::utf8_path::path_str(path)
    }

    pub fn osc_str(&self) -> Result<&str, DevError> {
        let path =
            self.inputs.osc_paths().first().ok_or_else(|| {
                DevError::Internal("pbfhogg command input has no OSC file".into())
            })?;
        crate::utf8_path::path_str(path)
    }

    pub fn osc_strs(&self) -> Result<Vec<&str>, DevError> {
        if self.inputs.osc_paths().is_empty() {
            return Err(DevError::Internal(
                "pbfhogg command input has no OSC files".into(),
            ));
        }
        self.inputs
            .osc_paths()
            .iter()
            .map(|path| crate::utf8_path::path_str(path))
            .collect()
    }

    pub fn pbf_b_str(&self) -> Result<&str, DevError> {
        let path = self
            .inputs
            .second_pbf()
            .ok_or_else(|| DevError::Internal("pbfhogg command input has no second PBF".into()))?;
        crate::utf8_path::path_str(path)
    }

    pub fn pbf_basename(&self) -> Result<String, DevError> {
        let path = self
            .inputs
            .primary_pbf()
            .ok_or_else(|| DevError::Internal("pbfhogg command input has no primary PBF".into()))?;
        Ok(crate::utf8_path::file_name(path))
    }
}
