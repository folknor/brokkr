use std::path::{Path, PathBuf};

pub(crate) mod cmd;
pub mod bench_all;
pub mod bench_node_store;
pub mod bench_planetiler;
pub mod bench_pmtiles;
pub mod bench_self;
pub mod bench_tilemaker;
pub mod compare_tiles;
pub mod download_ocean;
pub mod hotpath;
pub mod profile;
pub mod verify;

/// Elivagar-specific pipeline options shared across bench, hotpath, and profile.
///
/// Groups the flags that control elivagar's tilegen pipeline behaviour. These
/// are passed through from CLI → cmd dispatch → module entry point without
/// being interpreted by brokkr itself.
pub struct PipelineOpts<'a> {
    pub no_ocean: bool,
    pub force_sorted: bool,
    pub allow_unsafe_flat_index: bool,
    pub tile_format: Option<&'a str>,
    pub tile_compression: Option<&'a str>,
    pub compress_sort_chunks: Option<&'a str>,
    pub in_memory: bool,
    pub locations_on_ways: bool,
}

impl PipelineOpts<'_> {
    /// Append elivagar CLI flags to an args vec.
    pub fn push_args(&self, args: &mut Vec<String>, data_dir: &Path) {
        if self.force_sorted {
            args.push("--force-sorted".into());
        }
        if self.allow_unsafe_flat_index {
            args.push("--allow-unsafe-flat-index".into());
        }
        if let Some(fmt) = self.tile_format {
            args.push("--tile-format".into());
            args.push(fmt.into());
        }
        if let Some(comp) = self.tile_compression {
            args.push("--tile-compression".into());
            args.push(comp.into());
        }
        if let Some(algo) = self.compress_sort_chunks {
            args.push("--compress-sort-chunks".into());
            args.push(algo.into());
        }
        if self.in_memory {
            args.push("--in-memory".into());
        }
        if self.locations_on_ways {
            args.push("--locations-on-ways".into());
        }
        push_ocean_args(args, data_dir, self.no_ocean);
    }

    /// Build common metadata KvPairs for benchmark storage.
    pub fn metadata(&self) -> Vec<crate::db::KvPair> {
        let mut m = vec![
            crate::db::KvPair::text("meta.ocean", (!self.no_ocean).to_string()),
            crate::db::KvPair::text("meta.force_sorted", self.force_sorted.to_string()),
            crate::db::KvPair::text(
                "meta.allow_unsafe_flat_index",
                self.allow_unsafe_flat_index.to_string(),
            ),
        ];
        if let Some(v) = self.tile_format {
            m.push(crate::db::KvPair::text("meta.tile_format", v));
        }
        if let Some(v) = self.tile_compression {
            m.push(crate::db::KvPair::text("meta.tile_compression", v));
        }
        m.push(crate::db::KvPair::text(
            "meta.compress_sort_chunks",
            self.compress_sort_chunks.unwrap_or("none"),
        ));
        m.push(crate::db::KvPair::text("meta.in_memory", self.in_memory.to_string()));
        m.push(crate::db::KvPair::text("meta.locations_on_ways_cli", self.locations_on_ways.to_string()));
        m
    }
}

/// Detect ocean shapefiles in the data directory.
///
/// Returns (full-resolution, simplified) paths if they exist.
pub fn detect_ocean(data_dir: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let full = data_dir
        .join("water-polygons-split-3857")
        .join("water_polygons.shp");
    let simplified = data_dir
        .join("simplified-water-polygons-split-3857")
        .join("simplified_water_polygons.shp");

    (
        full.exists().then_some(full),
        simplified.exists().then_some(simplified),
    )
}

/// Check elivagar's stderr for evidence of LocationsOnWays runtime detection.
///
/// Elivagar prints "LocationsOnWays" to stderr when it detects the feature
/// from the PBF header (or CLI flag). This is the source of truth for whether
/// the locations-on-ways code path was actually used.
pub fn detect_locations_on_ways_stderr(stderr: &[u8]) -> bool {
    // Fast byte search — avoids UTF-8 conversion.
    stderr
        .windows(b"LocationsOnWays".len())
        .any(|w| w == b"LocationsOnWays")
}

/// Detect ocean shapefiles and push `--ocean`/`--ocean-simplified` args.
pub fn push_ocean_args(args: &mut Vec<String>, data_dir: &Path, no_ocean: bool) {
    if no_ocean {
        return;
    }
    let (ocean, simplified) = detect_ocean(data_dir);
    if let Some(ref shp) = ocean {
        args.push("--ocean".into());
        args.push(shp.display().to_string());
    }
    if let Some(ref shp) = simplified {
        args.push("--ocean-simplified".into());
        args.push(shp.display().to_string());
    }
}
