use std::path::{Path, PathBuf};

pub mod bench_all;
pub mod bench_node_store;
pub mod bench_planetiler;
pub mod bench_pmtiles;
pub mod bench_self;
pub mod bench_tilemaker;
pub(crate) mod cmd;
pub mod commands;
pub mod compare_tiles;
pub mod dispatch;
pub mod download_natural_earth;
pub mod download_ocean;
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
    pub fanout_cap_default: Option<u32>,
    pub fanout_cap: Option<&'a str>,
    pub polygon_simplify_factor: Option<f64>,
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
        if let Some(n) = self.fanout_cap_default {
            args.push("--fanout-cap-default".into());
            args.push(n.to_string());
        }
        if let Some(spec) = self.fanout_cap {
            args.push("--fanout-cap".into());
            args.push(spec.into());
        }
        if let Some(f) = self.polygon_simplify_factor {
            args.push("--polygon-simplify-factor".into());
            args.push(f.to_string());
        }
        push_ocean_args(args, data_dir, self.no_ocean);
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
