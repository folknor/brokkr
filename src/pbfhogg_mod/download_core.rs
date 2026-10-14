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

const PLANET_ORIGIN: &str = "planet.openstreetmap.org";
const PLANET_BASE_URL: &str = "https://planet.openstreetmap.org";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CivilDate {
    year: i32,
    month: u32,
    day: u32,
}

impl CivilDate {
    fn from_unix_seconds(unix_secs: i64) -> Self {
        Self::from_unix_days(unix_secs.div_euclid(86_400))
    }

    fn from_unix_days(days: i64) -> Self {
        let z = days + 719_468;
        let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let doe = (z - era * 146_097) as u32;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let year = i64::from(yoe) + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if month <= 2 { year + 1 } else { year };
        #[allow(clippy::cast_possible_truncation)]
        let year = year as i32;
        Self { year, month, day }
    }

    fn parse_yyyymmdd(value: &str) -> Option<Self> {
        if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let year = value[..4].parse().ok()?;
        let month = value[4..6].parse().ok()?;
        let day = value[6..8].parse().ok()?;
        Self::new(year, month, day)
    }

    fn parse_iso(value: &str) -> Option<Self> {
        let bytes = value.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return None;
        }
        let year = value.get(..4)?;
        let month = value.get(5..7)?;
        let day = value.get(8..10)?;
        if !year.bytes().all(|byte| byte.is_ascii_digit())
            || !month.bytes().all(|byte| byte.is_ascii_digit())
            || !day.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        Self::new(year.parse().ok()?, month.parse().ok()?, day.parse().ok()?)
    }

    fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        let days_in_month = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
            2 => 28,
            _ => return None,
        };
        if !(1..=days_in_month).contains(&day) {
            return None;
        }
        Some(Self { year, month, day })
    }

    fn yyyymmdd(self) -> String {
        format!("{:04}{:02}{:02}", self.year, self.month, self.day)
    }

    fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    fn unix_seconds(self) -> i64 {
        let year = i64::from(self.year) - if self.month <= 2 { 1 } else { 0 };
        let era = (if year >= 0 { year } else { year - 399 }) / 400;
        #[allow(clippy::cast_sign_loss)]
        let yoe = (year - era * 400) as u64;
        let month = u64::from(self.month);
        let day = u64::from(self.day);
        let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        #[allow(clippy::cast_possible_wrap)]
        let days = doe as i64;
        (era * 146_097 + days - 719_468) * 86_400
    }
}

/// Today's date as `YYYYMMDD`.
fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    #[allow(clippy::cast_possible_wrap)]
    let secs = secs as i64;
    CivilDate::from_unix_seconds(secs).yyyymmdd()
}

/// Convert days since 1970-01-01 to (year, month, day).
/// Algorithm from Howard Hinnant's chrono-compatible date library.
fn days_to_civil(days: i64) -> (i32, u32, u32) {
    let date = CivilDate::from_unix_days(days);
    (date.year, date.month, date.day)
}

fn raw_pbf_filename(dataset_key: &str, date_or_snapshot: &str) -> String {
    format!("{dataset_key}-{date_or_snapshot}.osm.pbf")
}

fn indexed_pbf_filename(dataset_key: &str, date_or_snapshot: &str) -> String {
    format!("{dataset_key}-{date_or_snapshot}-with-indexdata.osm.pbf")
}

fn snapshot_pbf_filename(dataset_key: &str, snap_key: &str, variant: &str) -> String {
    if variant == "indexed" {
        indexed_pbf_filename(dataset_key, snap_key)
    } else {
        raw_pbf_filename(dataset_key, snap_key)
    }
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
                format!("{PLANET_BASE_URL}/pbf/planet-latest.osm.pbf")
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
                format!("{PLANET_BASE_URL}/replication/day/{sp}.osc.gz")
            }
        }
    }

    /// Origin string for `brokkr.toml`.
    fn origin(&self) -> &'static str {
        match self {
            Self::Geofabrik { .. } => "Geofabrik",
            Self::Planet => PLANET_ORIGIN,
        }
    }

    /// Display name for log messages.
    fn display_name(&self) -> String {
        match self {
            Self::Geofabrik { path } => format!("Geofabrik: {path}"),
            Self::Planet => PLANET_ORIGIN.into(),
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
        && ds.origin.as_deref() == Some(PLANET_ORIGIN)
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
    data_dir.join(raw_pbf_filename(dataset_key, date))
}

/// Find the "indexed" PBF path - either the configured `indexed` variant, or
/// a dated filename as fallback.
fn indexed_pbf_path(dataset: Option<&Dataset>, data_dir: &Path, dataset_key: &str, date: &str) -> PathBuf {
    if let Some(ds) = dataset
        && let Some(entry) = ds.pbf.get("indexed")
    {
        return data_dir.join(&entry.file);
    }
    data_dir.join(indexed_pbf_filename(dataset_key, date))
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

    let target_filename = snapshot_pbf_filename(dataset_key, snap_key, target_variant);
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
        let hash = preflight::cached_path_digest(&target_path, project_root)?.to_string();

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

