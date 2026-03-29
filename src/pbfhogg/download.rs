//! Download region datasets and OSC diffs.
//!
//! Supports two sources:
//! - **Geofabrik**: regional extracts and diffs. Accepts short aliases
//!   (`denmark`, `europe`) or full paths (`europe/france`, `asia/japan/kanto`).
//! - **Planet**: full planet PBF and daily replication diffs from
//!   planet.openstreetmap.org.
//!
//! The source is determined automatically: `planet` maps to the planet
//! endpoint, everything else is Geofabrik. If a dataset already exists in
//! `brokkr.toml` with `origin = "planet.openstreetmap.org"`, the planet
//! source is used regardless of the region argument.

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

/// Format a 9-digit zero-padded sequence number as a 3-level path: `000/004/715`.
fn seq_path(seq: u64) -> String {
    let padded = format!("{seq:09}");
    let (a, rest) = padded.split_at(3);
    let (b, c) = rest.split_at(3);
    format!("{a}/{b}/{c}")
}

// ---------------------------------------------------------------------------
// Download source
// ---------------------------------------------------------------------------

/// Where to download PBF and OSC files from.
enum DownloadSource {
    /// Geofabrik regional extract. Path is e.g. `europe/denmark`.
    Geofabrik { path: String },
    /// Full planet from planet.openstreetmap.org.
    Planet,
}

impl DownloadSource {
    /// URL for the latest PBF.
    fn pbf_url(&self) -> String {
        match self {
            Self::Geofabrik { path } => {
                format!("https://download.geofabrik.de/{path}-latest.osm.pbf")
            }
            Self::Planet => {
                "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf".into()
            }
        }
    }

    /// URL for an OSC diff at the given sequence number.
    fn osc_url(&self, seq: u64) -> String {
        let sp = seq_path(seq);
        match self {
            Self::Geofabrik { path } => {
                format!("https://download.geofabrik.de/{path}-updates/{sp}.osc.gz")
            }
            Self::Planet => {
                format!("https://planet.openstreetmap.org/replication/day/{sp}.osc.gz")
            }
        }
    }

    /// Origin string for `brokkr.toml`.
    fn origin(&self) -> &'static str {
        match self {
            Self::Geofabrik { .. } => "Geofabrik",
            Self::Planet => "planet.openstreetmap.org",
        }
    }

    /// Display name for log messages.
    fn display_name(&self) -> String {
        match self {
            Self::Geofabrik { path } => format!("Geofabrik: {path}"),
            Self::Planet => "planet.openstreetmap.org".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Region resolution
// ---------------------------------------------------------------------------

/// Short aliases for commonly used regions.
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

/// Resolved download target.
struct ResolvedDownload {
    source: DownloadSource,
    /// Dataset key for `brokkr.toml` (e.g. `denmark`, `planet`).
    dataset_key: String,
}

/// Resolve a region argument into a download source and dataset key.
///
/// Resolution order:
/// 1. If an existing dataset has `origin = "planet.openstreetmap.org"`, use planet source.
/// 2. `"planet"` → planet source.
/// 3. Short alias → Geofabrik source.
/// 4. Path containing `/` → direct Geofabrik path.
/// 5. Error with suggestions.
fn resolve(name: &str, dataset: Option<&Dataset>) -> Result<ResolvedDownload, DevError> {
    // If the dataset already exists, let its origin override.
    if let Some(ds) = dataset {
        if ds.origin.as_deref() == Some("planet.openstreetmap.org") {
            return Ok(ResolvedDownload {
                source: DownloadSource::Planet,
                dataset_key: name.to_string(),
            });
        }
    }

    // "planet" keyword.
    if name == "planet" {
        return Ok(ResolvedDownload {
            source: DownloadSource::Planet,
            dataset_key: "planet".into(),
        });
    }

    // Short aliases.
    for &(alias, path) in ALIASES {
        if alias == name {
            return Ok(ResolvedDownload {
                source: DownloadSource::Geofabrik { path: path.into() },
                dataset_key: name.to_string(),
            });
        }
    }

    // Direct Geofabrik path.
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
        return Ok(ResolvedDownload {
            source: DownloadSource::Geofabrik { path: trimmed.into() },
            dataset_key,
        });
    }

    let alias_list: Vec<&str> = ALIASES.iter().map(|&(n, _)| n).collect();
    Err(DevError::Config(format!(
        "unknown region '{name}'. use 'planet', a Geofabrik path (e.g. europe/france), \
         or one of: {}",
        alias_list.join(", ")
    )))
}

// ---------------------------------------------------------------------------
// Existing-file checks
// ---------------------------------------------------------------------------

/// Check whether any configured PBF variant file already exists in the data dir.
/// Prefers `raw` variant since that's the best input for indexing.
fn has_existing_pbf(dataset: Option<&Dataset>, data_dir: &Path) -> Option<PathBuf> {
    let ds = dataset?;
    // Check raw first.
    if let Some(entry) = ds.pbf.get("raw") {
        let path = data_dir.join(&entry.file);
        if is_nonempty(&path) {
            return Some(path);
        }
    }
    // Fall back to any variant.
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
    origin: &str,
) -> Result<(), DevError> {
    let toml_path = project_root.join("brokkr.toml");
    let block = format!(
        "\n[{hostname}.datasets.{dataset_key}]\n\
         origin = \"{origin}\"\n"
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
    // Resolve region first to get dataset_key, then look up existing dataset,
    // then re-resolve with dataset context (so origin field can override source).
    let preliminary = resolve(region, None)?;
    let dataset = datasets.get(&preliminary.dataset_key);
    let resolved = resolve(region, dataset)?;

    let source = &resolved.source;
    let dataset_key = &resolved.dataset_key;
    let date = today();

    tools::check_curl()?;

    std::fs::create_dir_all(data_dir)?;

    output::download_msg(&format!("=== {dataset_key} ({}) ===", source.display_name()));

    let is_new_dataset = dataset.is_none();

    // -- Download PBF --
    let pbf_url = source.pbf_url();
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

                let url = source.osc_url(seq);
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

        let cat_input = has_existing_pbf(dataset, data_dir)
            .unwrap_or_else(|| pbf_dest.clone());

        let binary = build::cargo_build(
            &build::BuildConfig::release(Some("pbfhogg-cli")),
            project_root,
        )?;
        let binary_str = binary.display().to_string();
        let cat_input_str = cat_input.display().to_string();
        let indexed_tmp = indexed_dest.with_extension("tmp");
        let indexed_tmp_str = indexed_tmp.display().to_string();

        let captured = output::run_captured(
            &binary_str,
            &[
                "cat",
                &cat_input_str,
                "--type",
                "node,way,relation",
                "-o",
                &indexed_tmp_str,
            ],
            project_root,
        )?;

        if let Err(e) = captured.check_success(&binary_str) {
            let _ = std::fs::remove_file(&indexed_tmp);
            return Err(e);
        }
        std::fs::rename(&indexed_tmp, &indexed_dest)?;
        generated_indexed = true;
    }

    // -- Update brokkr.toml with new entries --
    let has_new_osc = !osc_downloaded.is_empty();
    if is_new_dataset && (downloaded_pbf || has_new_osc || generated_indexed) {
        append_dataset_header(project_root, hostname, dataset_key, source.origin())?;
    }

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

    if downloaded_pbf {
        output::download_msg(
            "  NOTE: run 'pbfhogg inspect <file>' to find the PBF sequence number, \
             then add seq = <N> to the brokkr.toml entry"
        );
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
