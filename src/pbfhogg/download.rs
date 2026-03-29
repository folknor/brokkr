//! Download region datasets from Geofabrik.
//!
//! Replaces `download-regions.sh`. Downloads the latest PBF, optionally an
//! OSC diff, and generates an indexed PBF variant via `pbfhogg cat`.
//!
//! Accepts either a short alias (`denmark`) or a full Geofabrik path
//! (`europe/denmark`, `asia/japan/kanto`). The dataset key in `brokkr.toml`
//! is always the last path component.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::build;
use crate::config::Dataset;
use crate::error::DevError;
use crate::output;
use crate::preflight;
use crate::tools;

/// Today's date as `YYYYMMDD`.
fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Days since epoch → year/month/day via civil calendar arithmetic.
    let days = (secs / 86400) as i64;
    let (y, m, d) = days_to_civil(days);
    format!("{y:04}{m:02}{d:02}")
}

/// Convert days since 1970-01-01 to (year, month, day).
/// Algorithm from Howard Hinnant's chrono-compatible date library.
fn days_to_civil(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

// ---------------------------------------------------------------------------
// Region resolution
// ---------------------------------------------------------------------------

/// Short aliases for commonly used regions, mapping to their Geofabrik path.
const ALIASES: &[(&str, &str)] = &[
    ("malta", "europe/malta"),
    ("greater-london", "europe/united-kingdom/england/greater-london"),
    ("switzerland", "europe/switzerland"),
    ("norway", "europe/norway"),
    ("japan", "asia/japan"),
    ("denmark", "europe/denmark"),
    ("germany", "europe/germany"),
    ("north-america", "north-america"),
    ("europe", "europe"),
];

/// Resolved region: the Geofabrik path and the dataset key for `brokkr.toml`.
struct ResolvedRegion {
    /// Full Geofabrik path (e.g. `europe/denmark`).
    geofabrik_path: String,
    /// Dataset key for `brokkr.toml` — last path component (e.g. `denmark`).
    dataset_key: String,
}

/// Resolve a region argument into a Geofabrik path and dataset key.
///
/// - If `name` matches a short alias, use the alias mapping.
/// - If `name` contains `/`, treat it as a direct Geofabrik path.
/// - Otherwise, suggest using a full path.
fn resolve_region(name: &str) -> Result<ResolvedRegion, DevError> {
    // Check aliases first.
    for &(alias, path) in ALIASES {
        if alias == name {
            return Ok(ResolvedRegion {
                geofabrik_path: path.to_string(),
                dataset_key: name.to_string(),
            });
        }
    }

    // Accept direct Geofabrik paths (anything with `/`).
    if name.contains('/') {
        let trimmed = name.trim_end_matches('/');
        let dataset_key = trimmed
            .rsplit('/')
            .next()
            .unwrap_or(trimmed)
            .to_string();
        if dataset_key.is_empty() {
            return Err(DevError::Config(format!(
                "cannot derive dataset key from path '{name}'"
            )));
        }
        return Ok(ResolvedRegion {
            geofabrik_path: trimmed.to_string(),
            dataset_key,
        });
    }

    let alias_list: Vec<&str> = ALIASES.iter().map(|&(n, _)| n).collect();
    Err(DevError::Config(format!(
        "unknown region '{name}'. use a Geofabrik path (e.g. europe/france) \
         or one of: {}",
        alias_list.join(", ")
    )))
}

/// Format a Geofabrik updates URL from a region path and sequence number.
///
/// Geofabrik encodes sequence numbers as 9-digit zero-padded paths split into
/// groups of 3: `000/004/715` for sequence 4715.
fn geofabrik_osc_url(geofabrik_path: &str, seq: u64) -> String {
    let padded = format!("{seq:09}");
    let (a, rest) = padded.split_at(3);
    let (b, c) = rest.split_at(3);
    format!(
        "https://download.geofabrik.de/{geofabrik_path}-updates/{a}/{b}/{c}.osc.gz"
    )
}

// ---------------------------------------------------------------------------
// Existing-file checks
// ---------------------------------------------------------------------------

/// Check whether any configured PBF variant file already exists in the data dir.
fn has_existing_pbf(dataset: Option<&Dataset>, data_dir: &Path) -> Option<PathBuf> {
    let ds = dataset?;
    for entry in ds.pbf.values() {
        let path = data_dir.join(&entry.file);
        if is_nonempty(&path) {
            return Some(path);
        }
    }
    None
}

/// Find the highest configured OSC sequence number in the dataset.
fn max_osc_seq(dataset: Option<&Dataset>) -> Option<u64> {
    let ds = dataset?;
    ds.osc.keys().filter_map(|k| k.parse::<u64>().ok()).max()
}

/// Check whether a configured OSC file for the given seq already exists.
fn has_existing_osc(dataset: Option<&Dataset>, data_dir: &Path, seq: u64) -> Option<PathBuf> {
    let ds = dataset?;
    let key = seq.to_string();
    let entry = ds.osc.get(&key)?;
    let path = data_dir.join(&entry.file);
    if is_nonempty(&path) {
        Some(path)
    } else {
        None
    }
}

/// Find the "raw" PBF path — either the configured `raw` variant, or a
/// dated filename as fallback.
fn raw_pbf_path(dataset: Option<&Dataset>, data_dir: &Path, dataset_key: &str, date: &str) -> PathBuf {
    if let Some(ds) = dataset {
        if let Some(entry) = ds.pbf.get("raw") {
            return data_dir.join(&entry.file);
        }
    }
    data_dir.join(format!("{dataset_key}-{date}.osm.pbf"))
}

/// Find the "indexed" PBF path — either the configured `indexed` variant, or
/// a dated filename as fallback.
fn indexed_pbf_path(dataset: Option<&Dataset>, data_dir: &Path, dataset_key: &str, date: &str) -> PathBuf {
    if let Some(ds) = dataset {
        if let Some(entry) = ds.pbf.get("indexed") {
            return data_dir.join(&entry.file);
        }
    }
    data_dir.join(format!("{dataset_key}-{date}-with-indexdata.osm.pbf"))
}

/// Build the OSC destination filename using the project naming convention.
fn osc_filename(dataset_key: &str, date: &str, seq: u64) -> String {
    format!("{dataset_key}-{date}-seq{seq}.osc.gz")
}

/// Check that a file exists and is not empty (guards against partial downloads).
fn is_nonempty(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > 0)
}

// ---------------------------------------------------------------------------
// TOML config updates
// ---------------------------------------------------------------------------

/// Append a new OSC entry to `brokkr.toml`.
fn append_osc_entry(
    project_root: &Path,
    hostname: &str,
    dataset_key: &str,
    seq: u64,
    filename: &str,
    xxhash: &str,
) -> Result<(), DevError> {
    let toml_path = project_root.join("brokkr.toml");
    let block = format!(
        "\n[{hostname}.datasets.{dataset_key}.osc.{seq}]\n\
         file = \"{filename}\"\n\
         xxhash = \"{xxhash}\"\n"
    );
    let mut contents = std::fs::read_to_string(&toml_path)?;
    contents.push_str(&block);
    std::fs::write(&toml_path, contents)?;
    output::download_msg(&format!(
        "  added [{hostname}.datasets.{dataset_key}.osc.{seq}] to brokkr.toml"
    ));
    Ok(())
}

/// Append a new PBF entry to `brokkr.toml`.
fn append_pbf_entry(
    project_root: &Path,
    hostname: &str,
    dataset_key: &str,
    variant: &str,
    filename: &str,
    xxhash: &str,
) -> Result<(), DevError> {
    let toml_path = project_root.join("brokkr.toml");
    let block = format!(
        "\n[{hostname}.datasets.{dataset_key}.pbf.{variant}]\n\
         file = \"{filename}\"\n\
         xxhash = \"{xxhash}\"\n"
    );
    let mut contents = std::fs::read_to_string(&toml_path)?;
    contents.push_str(&block);
    std::fs::write(&toml_path, contents)?;
    output::download_msg(&format!(
        "  added [{hostname}.datasets.{dataset_key}.pbf.{variant}] to brokkr.toml"
    ));
    Ok(())
}

/// Append a new dataset header to `brokkr.toml` if the dataset doesn't exist yet.
fn append_dataset_header(
    project_root: &Path,
    hostname: &str,
    dataset_key: &str,
) -> Result<(), DevError> {
    let toml_path = project_root.join("brokkr.toml");
    let block = format!(
        "\n[{hostname}.datasets.{dataset_key}]\n\
         origin = \"Geofabrik\"\n"
    );
    let mut contents = std::fs::read_to_string(&toml_path)?;
    contents.push_str(&block);
    std::fs::write(&toml_path, contents)?;
    output::download_msg(&format!(
        "  added [{hostname}.datasets.{dataset_key}] to brokkr.toml"
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(
    region: &str,
    osc_seq: Option<u64>,
    datasets: &std::collections::HashMap<String, Dataset>,
    hostname: &str,
    data_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let resolved = resolve_region(region)?;
    let dataset_key = &resolved.dataset_key;
    let dataset = datasets.get(dataset_key);
    let date = today();

    tools::check_curl()?;

    std::fs::create_dir_all(data_dir)?;

    output::download_msg(&format!("=== {dataset_key} (Geofabrik: {}) ===", resolved.geofabrik_path));

    let is_new_dataset = dataset.is_none();

    // -- Download PBF --
    let pbf_url = format!(
        "https://download.geofabrik.de/{}-latest.osm.pbf",
        resolved.geofabrik_path
    );
    let pbf_dest = raw_pbf_path(dataset, data_dir, dataset_key, &date);
    let mut downloaded_pbf = false;

    if let Some(existing) = has_existing_pbf(dataset, data_dir) {
        output::download_msg(&format!("  SKIP (exists): {}", existing.display()));
    } else if is_nonempty(&pbf_dest) {
        output::download_msg(&format!("  SKIP (exists): {}", pbf_dest.display()));
    } else {
        output::download_msg(&format!("  GET: {pbf_url}"));
        tools::download_file(&pbf_url, &pbf_dest)?;
        downloaded_pbf = true;
    }

    // -- Download OSC diffs --
    // Downloads all missing diffs from (last_configured + 1) through the requested seq.
    let mut osc_downloaded: Vec<(u64, PathBuf)> = Vec::new();
    let mut osc_last_dest: Option<PathBuf> = None;

    if let Some(target_seq) = osc_seq {
        let start_seq = max_osc_seq(dataset).map_or(target_seq, |max| max + 1);

        if start_seq > target_seq {
            output::download_msg(&format!(
                "  SKIP: OSC seqs up to {target_seq} already configured"
            ));
        } else {
            if start_seq < target_seq {
                output::download_msg(&format!(
                    "  downloading OSC diffs {start_seq}..{target_seq} ({} files)",
                    target_seq - start_seq + 1
                ));
            }

            for seq in start_seq..=target_seq {
                if has_existing_osc(dataset, data_dir, seq).is_some() {
                    continue;
                }

                let url = geofabrik_osc_url(&resolved.geofabrik_path, seq);
                let dest = data_dir.join(osc_filename(dataset_key, &date, seq));

                if dest.exists() && is_nonempty(&dest) {
                    output::download_msg(&format!("  SKIP (exists): {}", dest.display()));
                } else {
                    output::download_msg(&format!("  GET: {url}"));
                    tools::download_file(&url, &dest)?;
                    osc_downloaded.push((seq, dest.clone()));
                }
                osc_last_dest = Some(dest);
            }
        }
    }

    // -- Generate indexed PBF --
    let indexed_dest = indexed_pbf_path(dataset, data_dir, dataset_key, &date);
    let mut generated_indexed = false;

    if indexed_dest.exists() && is_nonempty(&indexed_dest) {
        output::download_msg(&format!("  SKIP (exists): {}", indexed_dest.display()));
    } else {
        output::download_msg("  generating indexed PBF via cat");

        // Use whichever raw PBF actually exists on disk for cat input.
        let cat_input = has_existing_pbf(dataset, data_dir)
            .unwrap_or_else(|| pbf_dest.clone());

        let binary = build::cargo_build(
            &build::BuildConfig::release(Some("pbfhogg-cli")),
            project_root,
        )?;
        let binary_str = binary.display().to_string();
        let cat_input_str = cat_input.display().to_string();
        let indexed_str = indexed_dest.display().to_string();

        let captured = output::run_captured(
            &binary_str,
            &[
                "cat",
                &cat_input_str,
                "--type",
                "node,way,relation",
                "-o",
                &indexed_str,
            ],
            project_root,
        )?;

        captured.check_success(&binary_str)?;
        generated_indexed = true;
    }

    // -- Update brokkr.toml with new entries --
    let has_new_osc = !osc_downloaded.is_empty();
    if is_new_dataset && (downloaded_pbf || has_new_osc || generated_indexed) {
        append_dataset_header(project_root, hostname, dataset_key)?;
    }

    // Only append entries that don't already exist in the config.
    let has_raw = dataset.is_some_and(|ds| ds.pbf.contains_key("raw"));
    let has_indexed = dataset.is_some_and(|ds| ds.pbf.contains_key("indexed"));

    if downloaded_pbf && !has_raw {
        let filename = pbf_dest
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        output::download_msg(&format!("  hashing {filename}..."));
        let hash = preflight::cached_xxh128(&pbf_dest, project_root)?;
        append_pbf_entry(project_root, hostname, dataset_key, "raw", &filename, &hash)?;
    }

    if generated_indexed && !has_indexed {
        let filename = indexed_dest
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        output::download_msg(&format!("  hashing {filename}..."));
        let hash = preflight::cached_xxh128(&indexed_dest, project_root)?;
        append_pbf_entry(project_root, hostname, dataset_key, "indexed", &filename, &hash)?;
    }

    for (seq, osc_path) in &osc_downloaded {
        // OSC entries are already guarded by has_existing_osc in the download
        // loop, so reaching here means the seq is not yet in the config.
        let filename = osc_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        output::download_msg(&format!("  hashing {filename}..."));
        let hash = preflight::cached_xxh128(osc_path, project_root)?;
        append_osc_entry(project_root, hostname, dataset_key, *seq, &filename, &hash)?;
    }

    // -- Summary --
    output::download_msg("=== Summary ===");
    output::download_msg(&format!("  PBF: {}", pbf_dest.display()));
    if let Some(ref osc) = osc_last_dest {
        if osc_downloaded.len() > 1 {
            output::download_msg(&format!(
                "  OSC: {} files downloaded ({} new entries in brokkr.toml)",
                osc_downloaded.len(),
                osc_downloaded.len(),
            ));
        } else {
            output::download_msg(&format!("  OSC: {}", osc.display()));
        }
    }
    output::download_msg(&format!("  Indexed: {}", indexed_dest.display()));

    Ok(())
}
