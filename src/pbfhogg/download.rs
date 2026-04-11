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
    #[allow(clippy::cast_possible_wrap)]
    let days = (secs / 86400) as i64; // safe: won't wrap until year 292 billion
    let (y, m, d) = days_to_civil(days);
    format!("{y:04}{m:02}{d:02}")
}

/// Convert days since 1970-01-01 to (year, month, day).
/// Algorithm from Howard Hinnant's chrono-compatible date library.
fn days_to_civil(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let doe = (z - era * 146097) as u32; // always 0..146096 by construction
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    #[allow(clippy::cast_possible_truncation)]
    let y_i32 = y as i32; // safe for dates anywhere near the present
    (y_i32, m, d)
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
    if let Some(ds) = dataset
        && ds.origin.as_deref() == Some("planet.openstreetmap.org")
    {
        return Ok(ResolvedDownload {
            source: DownloadSource::Planet,
            dataset_key: name.to_string(),
        });
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
    if let Some(ds) = dataset
        && let Some(entry) = ds.pbf.get("raw")
    {
        return data_dir.join(&entry.file);
    }
    data_dir.join(format!("{dataset_key}-{date}.osm.pbf"))
}

/// Find the "indexed" PBF path — either the configured `indexed` variant, or
/// a dated filename as fallback.
fn indexed_pbf_path(dataset: Option<&Dataset>, data_dir: &Path, dataset_key: &str, date: &str) -> PathBuf {
    if let Some(ds) = dataset
        && let Some(entry) = ds.pbf.get("indexed")
    {
        return data_dir.join(&entry.file);
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

/// Append a new snapshot header `[host.datasets.<dataset>.snapshot.<key>]` to `brokkr.toml`.
fn append_snapshot_header(
    project_root: &Path,
    hostname: &str,
    dataset_key: &str,
    snapshot_key: &str,
    date: &str,
) -> Result<(), DevError> {
    let toml_path = project_root.join("brokkr.toml");
    let block = format!(
        "\n[{hostname}.datasets.{dataset_key}.snapshot.{snapshot_key}]\n\
         download_date = \"{date}\"\n"
    );
    let mut contents = std::fs::read_to_string(&toml_path)?;
    contents.push_str(&block);
    std::fs::write(&toml_path, contents)?;
    output::download_msg(&format!(
        "  added [{hostname}.datasets.{dataset_key}.snapshot.{snapshot_key}] to brokkr.toml"
    ));
    Ok(())
}

/// Append a snapshot PBF entry `[...snapshot.<key>.pbf.<variant>]` to `brokkr.toml`.
fn append_snapshot_pbf_entry(
    project_root: &Path,
    hostname: &str,
    dataset_key: &str,
    snapshot_key: &str,
    variant: &str,
    filename: &str,
    xxhash: &str,
) -> Result<(), DevError> {
    let toml_path = project_root.join("brokkr.toml");
    let block = format!(
        "\n[{hostname}.datasets.{dataset_key}.snapshot.{snapshot_key}.pbf.{variant}]\n\
         file = \"{filename}\"\n\
         xxhash = \"{xxhash}\"\n"
    );
    let mut contents = std::fs::read_to_string(&toml_path)?;
    contents.push_str(&block);
    std::fs::write(&toml_path, contents)?;
    output::download_msg(&format!(
        "  added [{hostname}.datasets.{dataset_key}.snapshot.{snapshot_key}.pbf.{variant}] to brokkr.toml"
    ));
    Ok(())
}

/// Rotate the dataset's primary pbf/osc table headers into a snapshot block.
///
/// Performs a line-based rewrite of `brokkr.toml`:
/// - Renames every `[<host>.datasets.<dataset>.pbf.<variant>]` header to
///   `[<host>.datasets.<dataset>.snapshot.<snap_key>.pbf.<variant>]`.
/// - Renames every `[<host>.datasets.<dataset>.osc.<seq>]` header to
///   `[<host>.datasets.<dataset>.snapshot.<snap_key>.osc.<seq>]`.
/// - Updates the `download_date` field inside the `[<host>.datasets.<dataset>]`
///   block to `new_download_date` (or inserts it right after the dataset
///   header line if absent).
///
/// Body lines (file = "...", xxhash = "...", seq = N, etc.) are preserved
/// unchanged. Comments and other dataset blocks are not touched.
///
/// This is line-based, not a TOML parser — it works only on brokkr-generated
/// TOML where each table starts with `[name]` on its own line. Hand-edited
/// TOMLs with unusual formatting may break it; that's a known limitation
/// documented in CLAUDE.md.
fn rotate_dataset_to_snapshot(
    project_root: &Path,
    hostname: &str,
    dataset_key: &str,
    snap_key: &str,
    new_download_date: &str,
) -> Result<(), DevError> {
    let toml_path = project_root.join("brokkr.toml");
    let contents = std::fs::read_to_string(&toml_path)?;

    let dataset_header = format!("[{hostname}.datasets.{dataset_key}]");
    let pbf_prefix = format!("[{hostname}.datasets.{dataset_key}.pbf.");
    let osc_prefix = format!("[{hostname}.datasets.{dataset_key}.osc.");
    let snap_pbf_prefix =
        format!("[{hostname}.datasets.{dataset_key}.snapshot.{snap_key}.pbf.");
    let snap_osc_prefix =
        format!("[{hostname}.datasets.{dataset_key}.snapshot.{snap_key}.osc.");

    let mut output = String::with_capacity(contents.len() + 256);
    let mut in_dataset_block = false;
    let mut updated_download_date = false;

    for line in contents.lines() {
        let trimmed = line.trim_start();

        // Track which block we're currently inside.
        if trimmed.starts_with('[') {
            // Did we just leave the dataset block without seeing download_date?
            // If so, insert it right after the header (this branch only fires
            // when entering a *new* block; we'll catch the entry case below).
            if in_dataset_block && !updated_download_date {
                output.push_str(&format!(
                    "download_date = \"{new_download_date}\"\n"
                ));
                updated_download_date = true;
            }
            in_dataset_block = trimmed == dataset_header;
        }

        // Rename pbf table headers: keep the leading whitespace from the
        // original line so indented TOML survives untouched.
        if trimmed.starts_with(&pbf_prefix) {
            let leading_ws = &line[..line.len() - trimmed.len()];
            let suffix = &trimmed[pbf_prefix.len()..];
            output.push_str(leading_ws);
            output.push_str(&snap_pbf_prefix);
            output.push_str(suffix);
            output.push('\n');
            continue;
        }
        if trimmed.starts_with(&osc_prefix) {
            let leading_ws = &line[..line.len() - trimmed.len()];
            let suffix = &trimmed[osc_prefix.len()..];
            output.push_str(leading_ws);
            output.push_str(&snap_osc_prefix);
            output.push_str(suffix);
            output.push('\n');
            continue;
        }

        // Inside the dataset block: replace existing download_date in place.
        if in_dataset_block && trimmed.starts_with("download_date") && trimmed.contains('=') {
            let leading_ws = &line[..line.len() - trimmed.len()];
            output.push_str(leading_ws);
            output.push_str(&format!("download_date = \"{new_download_date}\""));
            output.push('\n');
            updated_download_date = true;
            continue;
        }

        output.push_str(line);
        output.push('\n');
    }

    // EOF case: dataset block was the last block and we never saw download_date.
    if in_dataset_block && !updated_download_date {
        output.push_str(&format!("download_date = \"{new_download_date}\"\n"));
    }

    std::fs::write(&toml_path, output)?;
    output::download_msg(&format!(
        "  rotated [{hostname}.datasets.{dataset_key}] pbf/osc tables → snapshot.{snap_key}"
    ));
    Ok(())
}

/// Append a snapshot OSC entry `[...snapshot.<key>.osc.<seq>]` to `brokkr.toml`.
fn append_snapshot_osc_entry(
    project_root: &Path,
    hostname: &str,
    dataset_key: &str,
    snapshot_key: &str,
    seq: u64,
    filename: &str,
    xxhash: &str,
) -> Result<(), DevError> {
    let toml_path = project_root.join("brokkr.toml");
    let block = format!(
        "\n[{hostname}.datasets.{dataset_key}.snapshot.{snapshot_key}.osc.{seq}]\n\
         file = \"{filename}\"\n\
         xxhash = \"{xxhash}\"\n"
    );
    let mut contents = std::fs::read_to_string(&toml_path)?;
    contents.push_str(&block);
    std::fs::write(&toml_path, contents)?;
    output::download_msg(&format!(
        "  added [{hostname}.datasets.{dataset_key}.snapshot.{snapshot_key}.osc.{seq}] to brokkr.toml"
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

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn run(
    region: &str,
    osc_seq: Option<u64>,
    as_snapshot: Option<&str>,
    refresh: bool,
    force: bool,
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

    // Snapshot mode short-circuits to a separate flow that registers the
    // download under [<host>.datasets.<key>.snapshot.<snap_key>] instead of
    // touching the dataset's primary pbf/osc tables.
    if let Some(snap_key) = as_snapshot {
        return run_as_snapshot(
            &resolved,
            snap_key,
            osc_seq,
            dataset,
            hostname,
            data_dir,
            project_root,
        );
    }

    // Refresh mode short-circuits to the rotation flow: archive existing
    // primary data into a snapshot block, then download new primary.
    if refresh {
        return run_refresh(
            &resolved,
            force,
            dataset,
            hostname,
            data_dir,
            project_root,
        );
    }

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
        let filename = existing
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        output::download_msg(&format!("  SKIP (pbf already configured): {filename}"));
        output::download_msg(
            "    └─ `brokkr download` does NOT auto-refresh existing primary data."
        );
        output::download_msg(&format!(
            "       To rotate to a newer upstream snapshot: `brokkr download {dataset_key} --refresh`"
        ));
        output::download_msg(
            "         (archives current primary as a snapshot block, downloads new primary).",
        );
        output::download_msg(&format!(
            "       To add a parallel named snapshot without rotating: `brokkr download {dataset_key} --as-snapshot <key>`"
        ));
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
            drop(std::fs::remove_file(&indexed_tmp));
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

/// Snapshot-mode download. Registers a new historical snapshot of an existing
/// dataset under `[<host>.datasets.<dataset>.snapshot.<key>]` instead of
/// touching the dataset's primary pbf/osc tables.
///
/// Errors if the dataset doesn't exist (with a suggested next command) or if
/// the snapshot key is already registered. Files use snapshot-specific names
/// (`{dataset}-{snapshot_key}.osm.pbf` etc.) so they don't collide with the
/// dataset's primary files on disk.
#[allow(clippy::too_many_lines)]
fn run_as_snapshot(
    resolved: &ResolvedDownload,
    snap_key: &str,
    osc_seq: Option<u64>,
    dataset: Option<&Dataset>,
    hostname: &str,
    data_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let source = &resolved.source;
    let dataset_key = &resolved.dataset_key;
    let date = today();

    // Q5: error if the dataset doesn't exist yet, with the next command to run.
    let ds = dataset.ok_or_else(|| {
        DevError::Config(format!(
            "Dataset '{dataset_key}' not found. Run `brokkr download {dataset_key}` to create the primary entry, \
             then `brokkr download {dataset_key} --as-snapshot {snap_key}` to add a snapshot."
        ))
    })?;

    // Reject if a snapshot with this key is already registered.
    if ds.snapshot.contains_key(snap_key) {
        return Err(DevError::Config(format!(
            "snapshot '{snap_key}' is already registered for dataset '{dataset_key}'. \
             Remove the [{hostname}.datasets.{dataset_key}.snapshot.{snap_key}] block from brokkr.toml first \
             if you want to re-download it."
        )));
    }

    tools::check_curl()?;
    std::fs::create_dir_all(data_dir)?;

    output::download_msg(&format!(
        "=== {dataset_key} snapshot '{snap_key}' ({}) ===",
        source.display_name()
    ));

    // Snapshot-specific filenames. The snapshot key is typically a date like
    // "20260411" but can be any [a-zA-Z0-9_-]+ string. Filenames bake the key
    // in directly so they don't collide with the dataset's primary files.
    let pbf_filename = format!("{dataset_key}-{snap_key}.osm.pbf");
    let pbf_dest = data_dir.join(&pbf_filename);
    let indexed_filename = format!("{dataset_key}-{snap_key}-with-indexdata.osm.pbf");
    let indexed_dest = data_dir.join(&indexed_filename);

    // -- Download PBF --
    let mut downloaded_pbf = false;
    if is_nonempty(&pbf_dest) {
        output::download_msg(&format!("  SKIP (exists): {}", pbf_dest.display()));
    } else {
        let url = source.pbf_url();
        output::download_msg(&format!("  GET: {url}"));
        tools::download_file(&url, &pbf_dest)?;
        downloaded_pbf = true;
    }

    // -- Download OSC diffs (snapshot-scoped, not anchored to legacy chain) --
    let mut osc_downloaded: Vec<(u64, PathBuf)> = Vec::new();
    if let Some(target_seq) = osc_seq {
        // Snapshot OSC chains start fresh — they're not extending the legacy chain.
        // Download every seq from min to target. Without a min lower bound we'd
        // download forever, so for now we require the user to invoke later with
        // a tighter range. Simplest: download just `target_seq` itself.
        // (Future enhancement: --osc-from N --osc-to M for snapshot-scoped ranges.)
        let url = source.osc_url(target_seq);
        let dest = data_dir.join(format!("{dataset_key}-{snap_key}-seq{target_seq}.osc.gz"));
        if is_nonempty(&dest) {
            output::download_msg(&format!("  SKIP (exists): {}", dest.display()));
        } else {
            output::download_msg(&format!("  GET: {url}"));
            tools::download_file(&url, &dest)?;
        }
        osc_downloaded.push((target_seq, dest));
    }

    // -- Generate indexed PBF --
    let mut generated_indexed = false;
    if is_nonempty(&indexed_dest) {
        output::download_msg(&format!("  SKIP (exists): {}", indexed_dest.display()));
    } else {
        output::download_msg("  generating indexed PBF via cat");
        let binary = build::cargo_build(
            &build::BuildConfig::release(Some("pbfhogg-cli")),
            project_root,
        )?;
        let binary_str = binary.display().to_string();
        let cat_input_str = pbf_dest.display().to_string();
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
            drop(std::fs::remove_file(&indexed_tmp));
            return Err(e);
        }
        std::fs::rename(&indexed_tmp, &indexed_dest)?;
        generated_indexed = true;
    }

    // -- Update brokkr.toml --
    // Always write the snapshot header (the snapshot is new — we errored
    // earlier if it already existed).
    //
    // The snapshot's `download_date` should reflect the snapshot's
    // point-in-time identity, NOT the date the user ran `brokkr download
    // --as-snapshot`. If the snapshot key parses as YYYYMMDD (the common case
    // when keys are dates like `20260411`), use that. Otherwise fall back to
    // today's date. Either way, format as YYYY-MM-DD to match the documented
    // schema.
    let snapshot_download_date = snapshot_key_to_iso_date(snap_key)
        .unwrap_or_else(|| iso_date_today(&date));
    append_snapshot_header(
        project_root,
        hostname,
        dataset_key,
        snap_key,
        &snapshot_download_date,
    )?;

    if downloaded_pbf || pbf_dest.exists() {
        let filename = pbf_dest.file_name().unwrap_or_default().to_string_lossy();
        output::download_msg(&format!("  hashing {filename}..."));
        let hash = preflight::cached_xxh128(&pbf_dest, project_root)?;
        append_snapshot_pbf_entry(
            project_root,
            hostname,
            dataset_key,
            snap_key,
            "raw",
            &filename,
            &hash,
        )?;
    }

    if downloaded_pbf {
        output::download_msg(
            "  NOTE: run 'pbfhogg inspect <file>' to find the PBF sequence number, \
             then add seq = <N> to the snapshot's pbf.raw entry"
        );
    }

    if generated_indexed || indexed_dest.exists() {
        let filename = indexed_dest
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        output::download_msg(&format!("  hashing {filename}..."));
        let hash = preflight::cached_xxh128(&indexed_dest, project_root)?;
        append_snapshot_pbf_entry(
            project_root,
            hostname,
            dataset_key,
            snap_key,
            "indexed",
            &filename,
            &hash,
        )?;
    }

    for (seq, osc_path) in &osc_downloaded {
        let filename = osc_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        output::download_msg(&format!("  hashing {filename}..."));
        let hash = preflight::cached_xxh128(osc_path, project_root)?;
        append_snapshot_osc_entry(
            project_root,
            hostname,
            dataset_key,
            snap_key,
            *seq,
            &filename,
            &hash,
        )?;
    }

    if !osc_downloaded.is_empty() {
        // Snapshot OSCs are consumable via `--snapshot <key>` on the OSC-aware
        // commands as of the C3 refresh feature. Hint at the invocation shape.
        output::download_msg(&format!(
            "  note: snapshot OSCs are addressable via `--snapshot {snap_key}` on \
             apply-changes/merge-changes/diff/diff-osc/tags-filter-osc"
        ));
    }

    // -- Summary --
    output::download_msg("=== Summary ===");
    output::download_msg(&format!("  PBF: {}", pbf_dest.display()));
    if let Some((_, last)) = osc_downloaded.last() {
        output::download_msg(&format!("  OSC: {}", last.display()));
    }
    output::download_msg(&format!("  Indexed: {}", indexed_dest.display()));
    output::download_msg(&format!(
        "  Use: brokkr diff-snapshots --dataset {dataset_key} --from base --to {snap_key}"
    ));

    Ok(())
}

/// Convert a unix epoch second timestamp into a `YYYYMMDD` date string (UTC).
/// Used to derive snapshot keys from file mtimes.
fn unix_to_yyyymmdd(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86400);
    let (y, m, d) = days_to_civil(days);
    format!("{y:04}{m:02}{d:02}")
}

/// If the snapshot key parses as a `YYYYMMDD` date string, convert it to
/// `YYYY-MM-DD` for writing as the snapshot's `download_date`. Returns `None`
/// if the key isn't an 8-digit date (e.g. `pre-refactor` or `staging-1`).
///
/// Used by `run_as_snapshot` to derive the snapshot's identity date from
/// its key when the key follows the common dated convention. Snapshots with
/// non-date keys fall back to today's date.
fn snapshot_key_to_iso_date(key: &str) -> Option<String> {
    if key.len() != 8 || !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i32 = key[..4].parse().ok()?;
    let m: u32 = key[4..6].parse().ok()?;
    let d: u32 = key[6..8].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// Convert a `YYYY-MM-DD` `download_date` string into `YYYYMMDD`. Returns
/// `None` if the input doesn't match the expected format.
fn iso_date_to_yyyymmdd(s: &str) -> Option<String> {
    if s.len() != 10 || s.as_bytes()[4] != b'-' || s.as_bytes()[7] != b'-' {
        return None;
    }
    let y = &s[..4];
    let m = &s[5..7];
    let d = &s[8..10];
    if y.bytes().all(|b| b.is_ascii_digit())
        && m.bytes().all(|b| b.is_ascii_digit())
        && d.bytes().all(|b| b.is_ascii_digit())
    {
        Some(format!("{y}{m}{d}"))
    } else {
        None
    }
}

/// Get the unix mtime of a file in seconds since epoch, or 0 on any error.
fn file_mtime_unix(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Refresh-mode download. Rotates the dataset's primary pbf/osc data into a
/// snapshot block (key derived from `download_date` or file mtime), then
/// downloads the new upstream PBF and resets the OSC chain.
///
/// HEAD-checks upstream `Last-Modified` first; if not newer than local,
/// no-ops with a message (unless `force` is set).
#[allow(clippy::too_many_lines)]
fn run_refresh(
    resolved: &ResolvedDownload,
    force: bool,
    dataset: Option<&Dataset>,
    hostname: &str,
    data_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let source = &resolved.source;
    let dataset_key = &resolved.dataset_key;

    // Validate dataset exists.
    let ds = dataset.ok_or_else(|| {
        DevError::Config(format!(
            "Dataset '{dataset_key}' not found. Run `brokkr download {dataset_key}` first to create the primary entry, \
             then `brokkr download {dataset_key} --refresh` to rotate to a newer snapshot."
        ))
    })?;

    // The legacy pbf.raw entry must exist — refresh rotates it into the snapshot.
    let legacy_raw = ds.pbf.get("raw").ok_or_else(|| {
        DevError::Config(format!(
            "dataset '{dataset_key}' has no pbf.raw entry to rotate. \
             Run `brokkr download {dataset_key}` first."
        ))
    })?;
    let legacy_raw_path = data_dir.join(&legacy_raw.file);
    if !is_nonempty(&legacy_raw_path) {
        return Err(DevError::Config(format!(
            "dataset '{dataset_key}' pbf.raw file is missing or empty: {}",
            legacy_raw_path.display()
        )));
    }

    tools::check_curl()?;

    output::download_msg(&format!(
        "=== {dataset_key} refresh ({}) ===",
        source.display_name()
    ));

    // -- Step 1: derive snapshot key for the archive --
    // Prefer download_date (formatted YYYYMMDD), fall back to legacy raw's mtime.
    let snap_key = ds
        .download_date
        .as_deref()
        .and_then(iso_date_to_yyyymmdd)
        .unwrap_or_else(|| {
            let mtime = file_mtime_unix(&legacy_raw_path);
            unix_to_yyyymmdd(mtime)
        });
    crate::config::validate_snapshot_key(&snap_key).map_err(|e| {
        DevError::Config(format!(
            "could not derive a valid snapshot key for the archived data: {e}. \
             Set `download_date = \"YYYY-MM-DD\"` in [{hostname}.datasets.{dataset_key}] and retry."
        ))
    })?;

    // Collision check: refuse if the snapshot key already exists.
    if ds.snapshot.contains_key(&snap_key) {
        return Err(DevError::Config(format!(
            "snapshot '{snap_key}' is already registered for dataset '{dataset_key}'. \
             Remove the [{hostname}.datasets.{dataset_key}.snapshot.{snap_key}] block from brokkr.toml first \
             if you want to rotate (and pick a different key by adjusting the dataset's download_date), \
             or back up the existing snapshot under a different key."
        )));
    }

    output::download_msg(&format!("  archive key: {snap_key}"));

    // -- Step 2: HEAD upstream Last-Modified, compare to local --
    let pbf_url = source.pbf_url();
    output::download_msg(&format!("  HEAD: {pbf_url}"));
    let head = tools::head_url(&pbf_url)?;
    let local_unix = ds
        .download_date
        .as_deref()
        .and_then(iso_date_parse_unix)
        .unwrap_or_else(|| file_mtime_unix(&legacy_raw_path));

    match head.last_modified_unix {
        Some(upstream_unix) if upstream_unix <= local_unix && !force => {
            output::download_msg(&format!(
                "  upstream Last-Modified ({}) is not newer than local ({}); no rotation needed.",
                unix_to_yyyymmdd(upstream_unix),
                unix_to_yyyymmdd(local_unix),
            ));
            output::download_msg(
                "  (use `--force` to rotate anyway, e.g. when the heuristic is wrong)",
            );
            return Ok(());
        }
        Some(upstream_unix) => {
            output::download_msg(&format!(
                "  upstream Last-Modified ({}) is newer than local ({}); proceeding with rotation",
                unix_to_yyyymmdd(upstream_unix),
                unix_to_yyyymmdd(local_unix),
            ));
        }
        None => {
            output::download_msg(
                "  upstream did not return a Last-Modified header; proceeding with rotation",
            );
        }
    }

    // -- Step 3: download new PBF to a fresh dated filename --
    std::fs::create_dir_all(data_dir)?;
    let date = today();
    let new_pbf_filename = format!("{dataset_key}-{date}.osm.pbf");
    let new_pbf_dest = data_dir.join(&new_pbf_filename);

    if is_nonempty(&new_pbf_dest) {
        // Defensive: don't clobber a freshly-downloaded file from a previous
        // half-completed refresh attempt.
        output::download_msg(&format!(
            "  SKIP download (exists): {}",
            new_pbf_dest.display()
        ));
    } else {
        output::download_msg(&format!("  GET: {pbf_url}"));
        tools::download_file(&pbf_url, &new_pbf_dest)?;
    }

    // -- Step 4: rotate TOML — rename existing pbf/osc tables into the snapshot block --
    rotate_dataset_to_snapshot(
        project_root,
        hostname,
        dataset_key,
        &snap_key,
        &iso_date_today(&date),
    )?;
    // Append the snapshot header itself with the OLD download_date.
    let old_download_date = ds
        .download_date
        .clone()
        .unwrap_or_else(|| iso_date_today(&unix_to_yyyymmdd(local_unix)));
    append_snapshot_header(
        project_root,
        hostname,
        dataset_key,
        &snap_key,
        &old_download_date,
    )?;

    // -- Step 5: hash and append the new top-level pbf.raw --
    output::download_msg(&format!("  hashing {new_pbf_filename}..."));
    let new_raw_hash = preflight::cached_xxh128(&new_pbf_dest, project_root)?;
    append_pbf_entry(
        project_root,
        hostname,
        dataset_key,
        "raw",
        &new_pbf_filename,
        &new_raw_hash,
    )?;
    output::download_msg(
        "  NOTE: run 'pbfhogg inspect <new pbf>' to find the new sequence number, \
         then add seq = <N> to the brokkr.toml entry"
    );

    // -- Step 6: regenerate the indexed PBF via pbfhogg cat --
    let new_indexed_filename = format!("{dataset_key}-{date}-with-indexdata.osm.pbf");
    let new_indexed_dest = data_dir.join(&new_indexed_filename);
    output::download_msg("  generating indexed PBF via cat");
    let binary = build::cargo_build(
        &build::BuildConfig::release(Some("pbfhogg-cli")),
        project_root,
    )?;
    let binary_str = binary.display().to_string();
    let cat_input_str = new_pbf_dest.display().to_string();
    let indexed_tmp = new_indexed_dest.with_extension("tmp");
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
        drop(std::fs::remove_file(&indexed_tmp));
        return Err(e);
    }
    std::fs::rename(&indexed_tmp, &new_indexed_dest)?;

    output::download_msg(&format!("  hashing {new_indexed_filename}..."));
    let indexed_hash = preflight::cached_xxh128(&new_indexed_dest, project_root)?;
    append_pbf_entry(
        project_root,
        hostname,
        dataset_key,
        "indexed",
        &new_indexed_filename,
        &indexed_hash,
    )?;

    // -- Summary --
    output::download_msg("=== Refresh complete ===");
    output::download_msg(&format!("  archived primary as snapshot.{snap_key}"));
    output::download_msg(&format!("  new primary PBF: {}", new_pbf_dest.display()));
    output::download_msg(&format!(
        "  new primary indexed: {}",
        new_indexed_dest.display()
    ));
    output::download_msg(&format!(
        "  Use: brokkr diff-snapshots --dataset {dataset_key} --from {snap_key} --to base"
    ));
    output::download_msg(&format!(
        "  Or: brokkr apply-changes --dataset {dataset_key} --snapshot {snap_key} --osc-seq <N>"
    ));

    Ok(())
}

/// Format `YYYYMMDD` as `YYYY-MM-DD` for writing to brokkr.toml.
fn iso_date_today(yyyymmdd: &str) -> String {
    if yyyymmdd.len() == 8 {
        format!("{}-{}-{}", &yyyymmdd[..4], &yyyymmdd[4..6], &yyyymmdd[6..8])
    } else {
        yyyymmdd.to_owned()
    }
}

/// Parse a `YYYY-MM-DD` string as a unix epoch second (UTC midnight).
/// Returns `None` if the format doesn't match.
fn iso_date_parse_unix(s: &str) -> Option<i64> {
    let yyyymmdd = iso_date_to_yyyymmdd(s)?;
    let y: i32 = yyyymmdd[..4].parse().ok()?;
    let m: u32 = yyyymmdd[4..6].parse().ok()?;
    let d: u32 = yyyymmdd[6..8].parse().ok()?;
    let days = civil_to_days(y, m, d)?;
    Some(days * 86400)
}

/// Convert (year, month, day) → days since 1970-01-01. Inverse of
/// `days_to_civil` from earlier in this file.
fn civil_to_days(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = y as i64;
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe as i64 - 719468)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_key_to_iso_date_basic() {
        // Date-shaped keys: convert.
        assert_eq!(
            snapshot_key_to_iso_date("20260411").as_deref(),
            Some("2026-04-11")
        );
        assert_eq!(
            snapshot_key_to_iso_date("19700101").as_deref(),
            Some("1970-01-01")
        );

        // Non-date keys: None (caller falls back to today).
        assert!(snapshot_key_to_iso_date("pre-refactor").is_none());
        assert!(snapshot_key_to_iso_date("staging-1").is_none());
        assert!(snapshot_key_to_iso_date("v1").is_none());

        // Wrong length.
        assert!(snapshot_key_to_iso_date("2026041").is_none());
        assert!(snapshot_key_to_iso_date("202604111").is_none());

        // Wrong characters (8 chars but not all digits).
        assert!(snapshot_key_to_iso_date("2026-411").is_none());

        // Out-of-range month/day.
        assert!(snapshot_key_to_iso_date("20261311").is_none());
        assert!(snapshot_key_to_iso_date("20260432").is_none());
        assert!(snapshot_key_to_iso_date("20260400").is_none());
        assert!(snapshot_key_to_iso_date("20260011").is_none());
    }

    #[test]
    fn iso_date_to_yyyymmdd_basic() {
        assert_eq!(iso_date_to_yyyymmdd("2026-04-11").as_deref(), Some("20260411"));
        assert_eq!(iso_date_to_yyyymmdd("2026-2-3"), None);
        assert_eq!(iso_date_to_yyyymmdd("not a date"), None);
        assert_eq!(iso_date_to_yyyymmdd("2026-04-1a"), None);
    }

    #[test]
    fn unix_to_yyyymmdd_round_trip() {
        // 2026-04-11T00:00:00Z = 1775865600
        assert_eq!(unix_to_yyyymmdd(1775865600), "20260411");
        // 1970-01-01T00:00:00Z = 0
        assert_eq!(unix_to_yyyymmdd(0), "19700101");
    }

    #[test]
    fn iso_date_parse_unix_basic() {
        assert_eq!(iso_date_parse_unix("2026-04-11"), Some(1775865600));
        assert_eq!(iso_date_parse_unix("1970-01-01"), Some(0));
        assert_eq!(iso_date_parse_unix("not a date"), None);
    }

    #[test]
    fn iso_date_today_formats_yyyymmdd_to_iso() {
        assert_eq!(iso_date_today("20260411"), "2026-04-11");
        // Non-8-char inputs pass through unchanged.
        assert_eq!(iso_date_today("2026-04-11"), "2026-04-11");
    }

    /// Helper: write a TOML to a temp file, run rotate_dataset_to_snapshot,
    /// read back, and return the new contents.
    fn run_rotation(
        before: &str,
        hostname: &str,
        dataset: &str,
        snap_key: &str,
        new_date: &str,
    ) -> String {
        let dir = std::env::current_dir()
            .unwrap()
            .join(".brokkr")
            .join("test-artifacts")
            .join(format!(
                "rotation-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        let toml_path = dir.join("brokkr.toml");
        std::fs::write(&toml_path, before).unwrap();

        rotate_dataset_to_snapshot(&dir, hostname, dataset, snap_key, new_date)
            .expect("rotate");

        let result = std::fs::read_to_string(&toml_path).unwrap();
        drop(std::fs::remove_dir_all(&dir));
        result
    }

    #[test]
    fn rotate_renames_pbf_and_osc_headers() {
        let before = "\
project = \"pbfhogg\"

[plantasjen.datasets.planet]
origin = \"planet.openstreetmap.org\"
download_date = \"2026-02-23\"

[plantasjen.datasets.planet.pbf.raw]
file = \"planet-20260223.osm.pbf\"
seq = 4912

[plantasjen.datasets.planet.pbf.indexed]
file = \"planet-20260223-with-indexdata.osm.pbf\"

[plantasjen.datasets.planet.osc.4913]
file = \"planet-20260223-seq4913.osc.gz\"
xxhash = \"abc\"
";
        let after = run_rotation(before, "plantasjen", "planet", "20260223", "2026-04-11");

        // Original pbf headers are renamed under snapshot.20260223.
        assert!(
            after.contains("[plantasjen.datasets.planet.snapshot.20260223.pbf.raw]"),
            "expected pbf.raw rename, got:\n{after}"
        );
        assert!(
            after.contains("[plantasjen.datasets.planet.snapshot.20260223.pbf.indexed]"),
            "expected pbf.indexed rename, got:\n{after}"
        );
        assert!(
            after.contains("[plantasjen.datasets.planet.snapshot.20260223.osc.4913]"),
            "expected osc rename, got:\n{after}"
        );

        // Original headers no longer present at the top level.
        assert!(
            !after.contains("\n[plantasjen.datasets.planet.pbf.raw]\n"),
            "old pbf.raw header should be gone, got:\n{after}"
        );
        assert!(
            !after.contains("\n[plantasjen.datasets.planet.osc.4913]\n"),
            "old osc.4913 header should be gone, got:\n{after}"
        );

        // Body lines preserved unchanged under the new headers.
        assert!(after.contains("file = \"planet-20260223.osm.pbf\""));
        assert!(after.contains("seq = 4912"));
        assert!(after.contains("xxhash = \"abc\""));

        // download_date in the [planet] block is updated to the new value.
        assert!(
            after.contains("download_date = \"2026-04-11\""),
            "expected updated download_date, got:\n{after}"
        );
        assert!(
            !after.contains("download_date = \"2026-02-23\""),
            "old download_date should be gone, got:\n{after}"
        );
    }

    #[test]
    fn rotate_inserts_download_date_when_missing() {
        let before = "\
project = \"pbfhogg\"

[plantasjen.datasets.planet]
origin = \"planet.openstreetmap.org\"

[plantasjen.datasets.planet.pbf.raw]
file = \"planet-20260223.osm.pbf\"
";
        let after = run_rotation(before, "plantasjen", "planet", "20260223", "2026-04-11");

        // download_date should be inserted somewhere in the [planet] block.
        assert!(
            after.contains("download_date = \"2026-04-11\""),
            "expected download_date inserted, got:\n{after}"
        );
    }

    #[test]
    fn rotate_does_not_touch_unrelated_datasets() {
        let before = "\
project = \"pbfhogg\"

[plantasjen.datasets.denmark]
origin = \"Geofabrik\"

[plantasjen.datasets.denmark.pbf.raw]
file = \"denmark-raw.osm.pbf\"

[plantasjen.datasets.planet]
origin = \"planet.openstreetmap.org\"
download_date = \"2026-02-23\"

[plantasjen.datasets.planet.pbf.raw]
file = \"planet-20260223.osm.pbf\"
";
        let after = run_rotation(before, "plantasjen", "planet", "20260223", "2026-04-11");

        // Denmark untouched.
        assert!(
            after.contains("[plantasjen.datasets.denmark.pbf.raw]"),
            "denmark pbf.raw should be untouched, got:\n{after}"
        );
        assert!(after.contains("file = \"denmark-raw.osm.pbf\""));

        // Planet rotated.
        assert!(after.contains("[plantasjen.datasets.planet.snapshot.20260223.pbf.raw]"));
    }
}
