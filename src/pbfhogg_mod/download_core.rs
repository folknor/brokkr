// Download region datasets and OSC diffs.
//
// Supports two sources:
// - **Geofabrik**: regional extracts and diffs. Accepts short aliases
//   (`denmark`, `europe`) or full paths (`europe/france`, `asia/japan/kanto`).
// - **Planet**: full planet PBF and daily replication diffs from
//   planet.openstreetmap.org.
//
// The source is determined automatically: `planet` maps to the planet
// endpoint, everything else is Geofabrik. If a dataset already exists in
// `brokkr.toml` with `origin = "planet.openstreetmap.org"`, the planet
// source is used regardless of the region argument.

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

/// Find the "raw" PBF path - either the configured `raw` variant, or a
/// dated filename as fallback.
fn raw_pbf_path(dataset: Option<&Dataset>, data_dir: &Path, dataset_key: &str, date: &str) -> PathBuf {
    if let Some(ds) = dataset
        && let Some(entry) = ds.pbf.get("raw")
    {
        return data_dir.join(&entry.file);
    }
    data_dir.join(format!("{dataset_key}-{date}.osm.pbf"))
}

/// Find the "indexed" PBF path - either the configured `indexed` variant, or
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
/// This is line-based, not a TOML parser - it works only on brokkr-generated
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
// Snapshot promotion (used by `brokkr repack` / `brokkr degrade --as-snapshot`)
// ---------------------------------------------------------------------------

/// Validate `--as-snapshot KEY` arguments before the build/run kicks off.
///
/// Catches the common mistake of forgetting `--replace-snapshot` on an
/// existing key: without this, the full pbfhogg run completes (potentially
/// after minutes of work) and only then errors out at registration. The
/// destructive replace path is still handled by `promote_snapshot` after the
/// run succeeds; this helper is non-destructive.
pub(crate) fn preflight_snapshot_collision(
    snap_key: &str,
    replace: bool,
    dataset_key: &str,
    dataset: Option<&Dataset>,
) -> Result<(), DevError> {
    crate::config::validate_snapshot_key(snap_key).map_err(DevError::Config)?;

    let ds = dataset.ok_or_else(|| {
        DevError::Config(format!(
            "dataset '{dataset_key}' is not registered. Run `brokkr download {dataset_key}` first to create the primary entry, \
             then re-run with `--as-snapshot {snap_key}`."
        ))
    })?;

    if ds.snapshot.contains_key(snap_key) && !replace {
        return Err(DevError::Config(format!(
            "snapshot '{snap_key}' is already registered for dataset '{dataset_key}'. \
             Pass `--replace-snapshot` to overwrite, or pick a different key."
        )));
    }

    Ok(())
}

/// Promote a generated PBF artifact into the dataset's snapshot graph.
///
/// Moves `scratch_pbf` into the dataset's `data_dir` under a stable filename,
/// computes its xxh128, and appends a `[..snapshot.<key>]` header plus a
/// `[..snapshot.<key>.pbf.<variant>]` entry to `brokkr.toml`. The `variant`
/// parameter is `"raw"` for `degrade --strip-indexdata` outputs (which carry
/// no indexdata) and `"indexed"` everywhere else.
///
/// Errors:
/// - `"base"` is reserved (CLI sentinel for the legacy top-level data).
/// - The dataset must already exist in `brokkr.toml`.
/// - The snapshot key must not already be registered, unless `replace = true`.
///   With `replace`, any existing snapshot blocks are stripped from the TOML
///   and any per-pbf files under the dataset's data dir are unlinked first.
#[allow(clippy::too_many_arguments)]
pub(crate) fn promote_snapshot(
    project_root: &Path,
    hostname: &str,
    dataset_key: &str,
    snap_key: &str,
    replace: bool,
    scratch_pbf: &Path,
    target_variant: &str,
    dataset: Option<&Dataset>,
    data_dir: &Path,
) -> Result<(), DevError> {
    crate::config::validate_snapshot_key(snap_key).map_err(DevError::Config)?;

    let ds = dataset.ok_or_else(|| {
        DevError::Config(format!(
            "dataset '{dataset_key}' is not registered. Run `brokkr download {dataset_key}` first to create the primary entry, \
             then re-run with `--as-snapshot {snap_key}`."
        ))
    })?;

    if ds.snapshot.contains_key(snap_key) {
        if replace {
            for entry in ds.snapshot[snap_key].pbf.values() {
                let p = data_dir.join(&entry.file);
                std::fs::remove_file(&p).ok();
            }
            for entry in ds.snapshot[snap_key].osc.values() {
                let p = data_dir.join(&entry.file);
                std::fs::remove_file(&p).ok();
            }
            remove_snapshot_blocks(project_root, hostname, dataset_key, snap_key)?;
        } else {
            return Err(DevError::Config(format!(
                "snapshot '{snap_key}' is already registered for dataset '{dataset_key}'. \
                 Pass `--replace-snapshot` to overwrite, or pick a different key."
            )));
        }
    }

    if !scratch_pbf.exists() {
        return Err(DevError::Config(format!(
            "expected scratch artifact at {} but the file is missing - did the run complete?",
            scratch_pbf.display(),
        )));
    }

    std::fs::create_dir_all(data_dir)?;

    let target_filename = match target_variant {
        "indexed" => format!("{dataset_key}-{snap_key}-with-indexdata.osm.pbf"),
        _ => format!("{dataset_key}-{snap_key}.osm.pbf"),
    };
    let target_path = data_dir.join(&target_filename);

    output::download_msg(&format!(
        "  promoting artifact -> {}",
        target_path.display()
    ));
    if let Err(e) = std::fs::rename(scratch_pbf, &target_path) {
        // rename() fails across filesystems with EXDEV; fall back to copy +
        // remove. Use the raw OS code so we don't depend on the
        // `ErrorKind::CrossesDevices` variant (recent stable only).
        if e.raw_os_error() == Some(libc::EXDEV) {
            std::fs::copy(scratch_pbf, &target_path)?;
            std::fs::remove_file(scratch_pbf).ok();
        } else {
            return Err(DevError::Io(e));
        }
    }

    let date = today();
    let snapshot_download_date = snapshot_key_to_iso_date(snap_key)
        .unwrap_or_else(|| iso_date_today(&date));
    append_snapshot_header(
        project_root,
        hostname,
        dataset_key,
        snap_key,
        &snapshot_download_date,
    )?;

    output::download_msg(&format!("  hashing {target_filename}..."));
    let hash = preflight::cached_xxh128(&target_path, project_root)?;
    append_snapshot_pbf_entry(
        project_root,
        hostname,
        dataset_key,
        snap_key,
        target_variant,
        &target_filename,
        &hash,
    )?;
    Ok(())
}

/// Strip every `[<host>.datasets.<dataset>.snapshot.<key>...]` block from
/// `brokkr.toml` (the snapshot header itself plus its pbf/osc sub-tables).
///
/// Line-based rewrite: drops every line inside a matched block until the
/// next `[` header. Other dataset blocks are untouched. Same caveats as
/// `rotate_dataset_to_snapshot` - works only on brokkr-generated TOML where
/// each header sits on its own line.
fn remove_snapshot_blocks(
    project_root: &Path,
    hostname: &str,
    dataset_key: &str,
    snap_key: &str,
) -> Result<(), DevError> {
    let toml_path = project_root.join("brokkr.toml");
    let contents = std::fs::read_to_string(&toml_path)?;

    let prefix = format!("[{hostname}.datasets.{dataset_key}.snapshot.{snap_key}");
    let mut out = String::with_capacity(contents.len());
    let mut dropping = false;

    for line in contents.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            // The match needs the next char after the prefix to be either
            // `]` (the snapshot header itself) or `.` (a sub-table). This
            // avoids accidentally matching an unrelated key whose name
            // happens to start with `<snap_key>`.
            if trimmed.starts_with(&prefix) {
                let rest = &trimmed[prefix.len()..];
                let starts_subblock = rest.starts_with(']') || rest.starts_with('.');
                if starts_subblock {
                    dropping = true;
                    continue;
                }
            }
            dropping = false;
        }
        if !dropping {
            out.push_str(line);
            out.push('\n');
        }
    }

    std::fs::write(&toml_path, out)?;
    output::download_msg(&format!(
        "  removed previous [{hostname}.datasets.{dataset_key}.snapshot.{snap_key}*] blocks from brokkr.toml"
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

