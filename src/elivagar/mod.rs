use std::path::Path;

use crate::config;
use crate::error::DevError;

pub mod bench_all;
pub mod bench_node_store;
pub mod bench_planetiler;
pub mod bench_pmtiles;
pub mod bench_self;
pub mod bench_tilemaker;
pub(crate) mod cmd;
pub mod commands;
pub mod compare_tiles;
pub mod bless;
pub mod diag;
pub mod dispatch;
pub mod download_natural_earth;
pub mod download_ocean;
pub mod inspect;
pub mod provenance;
pub mod regress;
pub mod svg;
pub mod verify;

/// Everything a tilegen run is configured by: the named `[<host>.tilegen.<name>]`
/// contract, plus the two assertions that belong to the input file rather than
/// to the config.
///
/// brokkr interprets none of this beyond expanding it into argv - but it does
/// name every input explicitly. Nothing is inferred from the filesystem. The
/// ocean inputs used to be auto-detected from `data/`, so a run's meaning lived
/// there rather than in the invocation: two runs of the same binary on the same
/// PBF produced different ocean geometry depending on whether a file happened
/// to exist, and `cli_args` could not tell you which. On 2026-07-14 a denmark
/// archive was built, verified and blessed as the regress baseline while
/// `ocean-tiles.pmtiles` was missing, taking the computed path throughout while
/// every gate passed.
pub struct PipelineOpts<'a> {
    pub tilegen: &'a config::TilegenConfig,
    /// From the selected `pbf.<variant>` entry - a property of the file.
    pub locations_on_ways: bool,
    /// From the selected `pbf.<variant>` entry - a property of the file.
    pub force_sorted: bool,
}

impl PipelineOpts<'_> {
    /// Append elivagar CLI flags to an args vec.
    ///
    /// Ocean paths are stored bare in brokkr.toml and resolved against
    /// `data_dir` here, like every other path in the file.
    ///
    /// A named input that cannot be honoured is an error rather than a
    /// fallback, so this returns `Err` for a missing ocean file instead of
    /// dropping it from the argv - dropping it is precisely how an archive
    /// gets built, gated and blessed on the computed path while its
    /// invocation claims otherwise.
    pub fn push_args(&self, args: &mut Vec<String>, data_dir: &Path) -> Result<(), DevError> {
        let tg = self.tilegen;

        if self.force_sorted {
            args.push("--force-sorted".into());
        }
        if self.locations_on_ways {
            args.push("--locations-on-ways".into());
        }
        if tg.allow_unsafe_flat_index {
            args.push("--allow-unsafe-flat-index".into());
        }
        if let Some(fmt) = &tg.tile_format {
            args.push("--tile-format".into());
            args.push(fmt.clone());
        }
        if let Some(comp) = &tg.tile_compression {
            args.push("--tile-compression".into());
            args.push(comp.clone());
        }
        if let Some(algo) = &tg.compress_sort_chunks {
            args.push("--compress-sort-chunks".into());
            args.push(algo.clone());
        }
        if tg.in_memory {
            args.push("--in-memory".into());
        }
        if let Some(n) = tg.threads {
            args.push("-j".into());
            args.push(n.to_string());
        }
        if let Some(s) = &tg.sort_budget {
            args.push("--sort-budget".into());
            args.push(s.clone());
        }
        if let Some(s) = &tg.way_budget {
            args.push("--way-budget".into());
            args.push(s.clone());
        }
        if let Some(s) = &tg.assemble_budget {
            args.push("--assemble-budget".into());
            args.push(s.clone());
        }
        if !tg.seam_reconcile_layers.is_empty() {
            let spec: Vec<String> = tg
                .seam_reconcile_layers
                .iter()
                .map(|(layer, maxzoom)| format!("{layer}:{maxzoom}"))
                .collect();
            args.push("--seam-reconcile-layers".into());
            args.push(spec.join(","));
        }
        if let Some(n) = tg.fanout_cap_default {
            args.push("--fanout-cap-default".into());
            args.push(n.to_string());
        }
        if !tg.fanout_caps.is_empty() {
            let spec: Vec<String> = tg
                .fanout_caps
                .iter()
                .map(|(layer, cap)| format!("{layer}={cap}"))
                .collect();
            args.push("--fanout-cap".into());
            args.push(spec.join(","));
        }
        if let Some(f) = tg.polygon_simplify_factor {
            args.push("--polygon-simplify-factor".into());
            args.push(f.to_string());
        }

        // Parsed and partition-checked at config load; re-parsed here to
        // resolve each spec's bare path against the data dir.
        let specs = tg
            .ocean_specs()
            .map_err(|e| DevError::Config(format!("tilegen ocean: {e}")))?;
        for spec in &specs {
            let path = data_dir.join(spec.file());
            if !path.exists() {
                return Err(DevError::Config(format!(
                    "tilegen ocean input not found: {} (named by brokkr.toml as '{}'); \
                     omitting it would silently change the ocean geometry this run \
                     produces",
                    path.display(),
                    spec.file()
                )));
            }
            args.push("--ocean".into());
            args.push(spec.render(&path.display().to_string()));
        }

        Ok(())
    }
}

/// Check elivagar's stderr for evidence of LocationsOnWays runtime detection.
///
/// Elivagar prints "LocationsOnWays" to stderr when it detects the feature
/// from the PBF header (or CLI flag). This is the source of truth for whether
/// the locations-on-ways code path was actually used.
pub fn detect_locations_on_ways_stderr(stderr: &[u8]) -> bool {
    // Fast byte search - avoids UTF-8 conversion.
    stderr
        .windows(b"LocationsOnWays".len())
        .any(|w| w == b"LocationsOnWays")
}

/// The tilegen block `tilegen` runs under. There is no selector flag yet.
pub const DEFAULT_TILEGEN: &str = "default";

/// Resolve the named tilegen contract for the current host.
///
/// A host with no `[<host>.tilegen.*]` is an error, not an implicit bare run.
/// A bare run - no ocean, stock everything - is a legitimate statement, and it
/// already has a spelling: an empty block. Inferring it from absent config
/// would put a run's meaning back outside the invocation, which is the whole
/// thing this design removes.
///
/// Resolves the hostname itself, mirroring `config::host_features`.
pub fn resolve_tilegen<'a>(
    dev_config: &'a config::DevConfig,
    name: &str,
) -> Result<&'a config::TilegenConfig, DevError> {
    let host = config::hostname()?;
    let blocks = dev_config
        .hosts
        .get(&host)
        .map(|h| &h.tilegen)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            DevError::Config(format!(
                "no [{host}.tilegen.*] blocks in brokkr.toml; tilegen is configured \
                 entirely from a named block, so add [{host}.tilegen.{name}] (an empty \
                 block is a valid no-ocean, stock-everything contract)"
            ))
        })?;

    blocks.get(name).ok_or_else(|| {
        let mut known: Vec<&str> = blocks.keys().map(String::as_str).collect();
        known.sort_unstable();
        DevError::Config(format!(
            "no [{host}.tilegen.{name}] in brokkr.toml (known: {})",
            known.join(", ")
        ))
    })
}

/// The two input assertions for a dataset variant, from its `pbf.<variant>`
/// entry: `(locations_on_ways, force_sorted)`.
///
/// Both describe the PBF rather than the pipeline, which is why they live on
/// the variant and not in a tilegen block - a block would otherwise have to
/// know which variant it was about to be run against, re-coupling the two axes
/// this config separates. An unknown dataset/variant yields `(false, false)`;
/// PBF resolution reports that properly and this is not the place for it.
pub fn input_assertions(dev_config: &config::DevConfig, dataset: &str, variant: &str) -> (bool, bool) {
    let Ok(host) = config::hostname() else {
        return (false, false);
    };
    dev_config
        .hosts
        .get(&host)
        .and_then(|h| h.datasets.get(dataset))
        .and_then(|d| d.pbf.get(variant))
        .map(|e| (e.locations_on_ways, e.force_sorted))
        .unwrap_or((false, false))
}
