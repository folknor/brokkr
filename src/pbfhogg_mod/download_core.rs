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
/// computes its xxh128, and registers a `[..snapshot.<key>]` header plus a
/// `[..snapshot.<key>.pbf.<variant>]` entry in `brokkr.toml`. The `variant`
/// parameter is `"raw"` for `degrade --strip-indexdata` outputs (which carry
/// no indexdata) and `"indexed"` everywhere else.
///
/// Errors:
/// - `"base"` is reserved (CLI sentinel for the legacy top-level data).
/// - The dataset must already exist in `brokkr.toml`.
/// - The snapshot key must not already be registered, unless `replace = true`.
///
/// Ordering is the safety property. Nothing destructive happens until the
/// new artefact exists: the scratch file is checked first, moved into place,
/// and hashed; then one atomic `brokkr.toml` commit swaps the old snapshot's
/// tables for the new ones; only after that commit are the replaced
/// snapshot's files unlinked (skipping any path the new registration, the
/// primary, or another snapshot still names). A failure at any step before
/// the commit leaves the old registration intact - including when the new
/// artefact's filename equals a replaced file's: that file is parked under a
/// hidden sibling name until the commit lands and restored if it does not.
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

    let replaced = ds.snapshot.get(snap_key);
    if replaced.is_some() && !replace {
        return Err(DevError::Config(format!(
            "snapshot '{snap_key}' is already registered for dataset '{dataset_key}'. \
             Pass `--replace-snapshot` to overwrite, or pick a different key."
        )));
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
    // A replaced snapshot commonly owns the very filename the new artefact
    // takes (the name derives from dataset, key and variant alone). Park it
    // beside the target until the commit lands, and put it back on any
    // failure, so a failed promotion never costs the registered bytes.
    let parked = data_dir.join(format!(".{target_filename}.replaced-{}", std::process::id()));
    let parked = if replaced.is_some() && target_path.exists() {
        std::fs::rename(&target_path, &parked)?;
        Some(parked)
    } else {
        None
    };

    let committed = (|| -> Result<(), DevError> {
        move_file_into_place(scratch_pbf, &target_path)?;

        output::download_msg(&format!("  hashing {target_filename}..."));
        let hash = preflight::cached_xxh128(&target_path, project_root)?;

        let date = today();
        let snapshot_download_date = snapshot_key_to_iso_date(snap_key)
            .unwrap_or_else(|| iso_date_today(&date));
        let mut toml = DatasetToml::open(project_root, hostname, dataset_key)?;
        if replaced.is_some() {
            toml.remove_snapshot(snap_key)?;
        }
        toml.set_snapshot_header(snap_key, &snapshot_download_date)?;
        toml.set_snapshot_pbf(snap_key, target_variant, &target_filename, &hash)?;
        toml.commit()
    })();
    if let Some(parked) = &parked {
        match &committed {
            Ok(()) => {
                std::fs::remove_file(parked).ok();
            }
            Err(_) => {
                if let Err(e) = std::fs::rename(parked, &target_path) {
                    output::warn(&format!(
                        "could not restore the replaced snapshot file from {}: {e}",
                        parked.display()
                    ));
                }
            }
        }
    }
    committed?;

    if let Some(old) = replaced {
        remove_replaced_snapshot_files(ds, snap_key, old, &target_filename, data_dir);
    }
    Ok(())
}

/// Unlink the files of a snapshot that `--replace-snapshot` just displaced.
///
/// Runs after the replacing registration is committed. A file is kept when
/// the new registration reuses its name, or when the primary tables or any
/// other snapshot still name it - deleting a file some surviving entry
/// points at would trade one stale registration for a broken one. Failures
/// are reported and skipped: the registration is already correct, and a
/// leftover file is clutter, not corruption.
fn remove_replaced_snapshot_files(
    ds: &Dataset,
    snap_key: &str,
    old: &crate::config::Snapshot,
    new_filename: &str,
    data_dir: &Path,
) {
    let still_named = |file: &str| {
        file == new_filename
            || ds.pbf.values().any(|e| e.file == file)
            || ds.osc.values().any(|e| e.file == file)
            || ds.snapshot.iter().any(|(key, snap)| {
                key != snap_key
                    && (snap.pbf.values().any(|e| e.file == file)
                        || snap.osc.values().any(|e| e.file == file))
            })
    };
    let old_files = old
        .pbf
        .values()
        .map(|e| e.file.as_str())
        .chain(old.osc.values().map(|e| e.file.as_str()));
    for file in old_files {
        if still_named(file) {
            // Named by the new registration or a surviving one: keep it.
            continue;
        }
        let path = data_dir.join(file);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => output::warn(&format!(
                "could not remove replaced snapshot file {}: {e}",
                path.display()
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

