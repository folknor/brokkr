//! Collision preflight and atomic promotion into the snapshot graph.

use super::date::{iso_date_today, snapshot_key_to_iso_date, today};
use super::layout::snapshot_pbf_filename;
use super::registry::{DatasetToml, move_file_into_place};
use crate::{config::Dataset, error::DevError, output, preflight};
use std::path::Path;

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
    let parked = data_dir.join(format!(
        ".{target_filename}.replaced-{}",
        std::process::id()
    ));
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
        let snapshot_download_date =
            snapshot_key_to_iso_date(snap_key).unwrap_or_else(|| iso_date_today(&date));
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
pub(super) fn remove_replaced_snapshot_files(
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
