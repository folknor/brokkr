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
