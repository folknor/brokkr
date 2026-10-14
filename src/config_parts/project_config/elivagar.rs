use crate::config::OceanSpec;
use serde::Deserialize;
use std::collections::BTreeMap;

pub use crate::elivagar::tilegen_config::{
    ByteSize, CompressionLevel, SortChunkCompression, TileCompression, TileFormat,
};

/// A named elivagar tilegen contract: `[<host>.tilegen.<name>]`.
///
/// This is the whole of what a tilegen run is configured by. Nothing is
/// inferred from the filesystem and there are no override flags - a run's
/// behaviour is a function of the named block and its input, and of nothing
/// else. Ocean inputs are explicit so the same binary and PBF cannot silently
/// select different geometry based on which files happen to be present in
/// `data/`.
///
/// Ordering matters here: `BTreeMap` (not `HashMap`) keeps the expanded argv
/// byte-identical across runs of the same block, which is what lets
/// `brokkr results --grep` select an arm off `cli_args`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TilegenConfig {
    /// Ocean inputs. Absent or empty means no ocean - the same statement
    /// elivagar's removed `--no-ocean` used to make.
    #[serde(default)]
    pub ocean: Vec<OceanSpec>,
    /// Gzip level 0-10. A *base*: elivagar clamps low zooms up and caps
    /// z13/z14 down, per `config.tile.compression_policy` in provenance.
    pub compression_level: Option<CompressionLevel>,
    pub tile_format: Option<TileFormat>,
    pub tile_compression: Option<TileCompression>,
    pub compress_sort_chunks: Option<SortChunkCompression>,
    #[serde(default)]
    pub in_memory: bool,
    /// `-j`. A host tuning knob, not part of the comparability contract -
    /// `reference/metadata.md` bars thread counts from the provenance block
    /// because same-commit builds must be byte-identical.
    pub threads: Option<u32>,
    /// Sizes accept `256M`, `1G`, or raw bytes.
    #[serde(
        default,
        deserialize_with = "crate::elivagar::tilegen_config::deserialize_sort_budget"
    )]
    pub sort_budget: Option<ByteSize>,
    #[serde(
        default,
        deserialize_with = "crate::elivagar::tilegen_config::deserialize_way_budget"
    )]
    pub way_budget: Option<ByteSize>,
    #[serde(
        default,
        deserialize_with = "crate::elivagar::tilegen_config::deserialize_assemble_budget"
    )]
    pub assemble_budget: Option<ByteSize>,
    /// Entry cap for the PMTiles payload dedup map. Plain integer, no size
    /// suffix. Absent = elivagar's 1M default. Outside the comparability
    /// contract: it changes how many duplicate payload copies the archive
    /// stores, never what a consumer decodes from a tile.
    pub dedup_cap: Option<u64>,
    /// Polygon layers getting shared-edge seam reconciliation, layer -> maxzoom.
    #[serde(default)]
    pub seam_reconcile_layers: BTreeMap<String, u32>,
    /// Default fanout cap for all polygon layers. 0 or absent = uncapped.
    pub fanout_cap_default: Option<u32>,
    /// Per-layer fanout caps; take precedence over the default.
    #[serde(default)]
    pub fanout_caps: BTreeMap<String, u32>,
    pub polygon_simplify_factor: Option<f64>,
    /// Expert debugging only; may cause severe IO/RSS degradation.
    #[serde(default)]
    pub allow_unsafe_flat_index: bool,
}

impl TilegenConfig {
    /// Return ocean entries parsed while loading the configuration.
    pub fn ocean_specs(&self) -> &[OceanSpec] {
        &self.ocean
    }
}

impl<'de> Deserialize<'de> for OceanSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let raw = String::deserialize(deserializer)?;
        let spec = OceanSpec::parse(&raw).map_err(D::Error::custom)?;
        if spec.file().trim().is_empty() {
            return Err(D::Error::custom("ocean input path is empty"));
        }
        Ok(spec)
    }
}
