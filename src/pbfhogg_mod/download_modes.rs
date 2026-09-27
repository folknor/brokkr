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
    build_root: &Path,
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
            build_root,
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
            build_root,
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
        let cat_input = has_existing_pbf(dataset, data_dir)
            .unwrap_or_else(|| pbf_dest.clone());
        generate_indexed_pbf(&cat_input, &indexed_dest, project_root, build_root)?;
        generated_indexed = true;
    }

    // -- Update brokkr.toml with new entries --
    // Hash everything first, then commit every entry in one atomic write.
    let mut toml = DatasetToml::open(project_root, hostname, dataset_key)?;
    let has_new_osc = !osc_downloaded.is_empty();
    if is_new_dataset && (downloaded_pbf || has_new_osc || generated_indexed) {
        toml.set_dataset_origin(source.origin())?;
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
        toml.set_pbf("raw", &filename, &hash)?;
    }

    if generated_indexed && !has_indexed {
        let filename = indexed_dest
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        output::download_msg(&format!("  hashing {filename}..."));
        let hash = preflight::cached_xxh128(&indexed_dest, project_root)?;
        toml.set_pbf("indexed", &filename, &hash)?;
    }

    for (seq, osc_path) in &osc_downloaded {
        let filename = osc_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        output::download_msg(&format!("  hashing {filename}..."));
        let hash = preflight::cached_xxh128(osc_path, project_root)?;
        toml.set_osc(*seq, &filename, &hash)?;
    }
    toml.commit()?;

    if downloaded_pbf {
        output::download_msg(
            "  NOTE: run 'pbfhogg inspect <file>' to find the PBF sequence number, \
             then add seq = <N> to the brokkr.toml entry"
        );
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
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn run_as_snapshot(
    resolved: &ResolvedDownload,
    snap_key: &str,
    osc_seq: Option<u64>,
    dataset: Option<&Dataset>,
    hostname: &str,
    data_dir: &Path,
    project_root: &Path,
    build_root: &Path,
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
        // Snapshot OSC chains start fresh - they're not extending the legacy chain.
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
        generate_indexed_pbf(&pbf_dest, &indexed_dest, project_root, build_root)?;
        generated_indexed = true;
    }

    // -- Update brokkr.toml --
    // Always write the snapshot header (the snapshot is new - we errored
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
    // Hash everything first, then commit the header and every entry in one
    // atomic write - a failed hash must not leave a header with no pbf.
    let mut toml = DatasetToml::open(project_root, hostname, dataset_key)?;
    toml.set_snapshot_header(snap_key, &snapshot_download_date)?;

    if downloaded_pbf || pbf_dest.exists() {
        let filename = pbf_dest.file_name().unwrap_or_default().to_string_lossy();
        output::download_msg(&format!("  hashing {filename}..."));
        let hash = preflight::cached_xxh128(&pbf_dest, project_root)?;
        toml.set_snapshot_pbf(snap_key, "raw", &filename, &hash)?;
    }

    if generated_indexed || indexed_dest.exists() {
        let filename = indexed_dest
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        output::download_msg(&format!("  hashing {filename}..."));
        let hash = preflight::cached_xxh128(&indexed_dest, project_root)?;
        toml.set_snapshot_pbf(snap_key, "indexed", &filename, &hash)?;
    }

    for (seq, osc_path) in &osc_downloaded {
        let filename = osc_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        output::download_msg(&format!("  hashing {filename}..."));
        let hash = preflight::cached_xxh128(osc_path, project_root)?;
        toml.set_snapshot_osc(snap_key, *seq, &filename, &hash)?;
    }
    toml.commit()?;

    if downloaded_pbf {
        output::download_msg(
            "  NOTE: run 'pbfhogg inspect <file>' to find the PBF sequence number, \
             then add seq = <N> to the snapshot's pbf.raw entry"
        );
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
#[allow(clippy::cast_possible_wrap)]
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
    build_root: &Path,
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

    // The legacy pbf.raw entry must exist - refresh rotates it into the snapshot.
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
    let new_indexed_filename = format!("{dataset_key}-{date}-with-indexdata.osm.pbf");
    let new_indexed_dest = data_dir.join(&new_indexed_filename);

    // The new primary's dated names must not be files the dataset already
    // registers. A refresh on the day of the previous download computes the
    // same names: the download would "resume" onto the old primary and `cat`
    // would overwrite the old indexed file that is about to be archived.
    let registered = |name: &str| {
        ds.pbf.values().any(|e| e.file == name)
            || ds.osc.values().any(|e| e.file == name)
            || ds.snapshot.values().any(|s| {
                s.pbf.values().any(|e| e.file == name) || s.osc.values().any(|e| e.file == name)
            })
    };
    for name in [&new_pbf_filename, &new_indexed_filename] {
        if registered(name.as_str()) {
            return Err(DevError::Config(format!(
                "refresh would write {name}, which dataset '{dataset_key}' already registers. \
                 Refreshing on the day of the previous download is not supported - retry tomorrow, \
                 or rename the registered file first."
            )));
        }
    }

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

    // -- Step 4: generate the new indexed PBF --
    // Before any brokkr.toml change: the rotation below is only committed
    // once every file the new primary names exists and is hashed.
    generate_indexed_pbf(&new_pbf_dest, &new_indexed_dest, project_root, build_root)?;

    // -- Step 5: hash the new primary --
    output::download_msg(&format!("  hashing {new_pbf_filename}..."));
    let new_raw_hash = preflight::cached_xxh128(&new_pbf_dest, project_root)?;
    output::download_msg(&format!("  hashing {new_indexed_filename}..."));
    let indexed_hash = preflight::cached_xxh128(&new_indexed_dest, project_root)?;

    // -- Step 6: one atomic brokkr.toml commit --
    // Archive the current primary pbf/osc tables under the snapshot (with the
    // OLD download_date), stamp the dataset with today's, and register the
    // new raw + indexed. Everything or nothing: a failure before this point
    // leaves the dataset's primary exactly as it was, and the downloaded
    // file is picked up by the SKIP branch above on retry.
    let old_download_date = ds
        .download_date
        .clone()
        .unwrap_or_else(|| iso_date_today(&unix_to_yyyymmdd(local_unix)));
    let mut toml = DatasetToml::open(project_root, hostname, dataset_key)?;
    toml.rotate_primary_to_snapshot(&snap_key, &old_download_date, &iso_date_today(&date))?;
    toml.set_pbf("raw", &new_pbf_filename, &new_raw_hash)?;
    toml.set_pbf("indexed", &new_indexed_filename, &indexed_hash)?;
    toml.commit()?;
    output::download_msg(
        "  NOTE: run 'pbfhogg inspect <new pbf>' to find the new sequence number, \
         then add seq = <N> to the brokkr.toml entry"
    );

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
    #[allow(clippy::cast_sign_loss)] // yoe is non-negative by construction
    let yoe = (y - era * 400) as u64;
    let m = u64::from(m);
    let d = u64::from(d);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    #[allow(clippy::cast_possible_wrap)]
    let days = doe as i64;
    Some(era * 146097 + days - 719468)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
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

    /// Write `contents` as `brokkr.toml` in a fresh scratch dir named by the
    /// calling test, and return the dir.
    fn toml_dir(test_name: &str, contents: &str) -> PathBuf {
        let dir = crate::test_scratch::scratch("pbfhogg-download", test_name);
        std::fs::write(dir.join("brokkr.toml"), contents).unwrap();
        dir
    }

    /// Run `edit` as one committed `DatasetToml` transaction on `before` and
    /// return the file's new contents.
    fn edit_toml(
        test_name: &str,
        before: &str,
        hostname: &str,
        dataset: &str,
        edit: impl FnOnce(&mut DatasetToml),
    ) -> String {
        let dir = toml_dir(test_name, before);
        let mut toml = DatasetToml::open(&dir, hostname, dataset).unwrap();
        edit(&mut toml);
        toml.commit().unwrap();
        std::fs::read_to_string(dir.join("brokkr.toml")).unwrap()
    }

    /// Rotation as `--refresh` performs it.
    fn run_rotation(
        test_name: &str,
        before: &str,
        hostname: &str,
        dataset: &str,
        snap_key: &str,
        old_date: &str,
        new_date: &str,
    ) -> String {
        edit_toml(test_name, before, hostname, dataset, |t| {
            t.rotate_primary_to_snapshot(snap_key, old_date, new_date)
                .expect("rotate");
        })
    }

    fn dataset_of<'a>(parsed: &'a toml::Value, host: &str, ds: &str) -> &'a toml::Value {
        &parsed[host]["datasets"][ds]
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
        let after = run_rotation(
            "rotate_renames",
            before,
            "plantasjen",
            "planet",
            "20260223",
            "2026-02-23",
            "2026-04-11",
        );

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

        // download_date in the [planet] block is updated to the new value;
        // the old one moves to the snapshot header.
        let parsed: toml::Value = toml::from_str(&after).unwrap();
        let ds = dataset_of(&parsed, "plantasjen", "planet");
        assert_eq!(ds["download_date"].as_str(), Some("2026-04-11"), "{after}");
        assert_eq!(
            ds["snapshot"]["20260223"]["download_date"].as_str(),
            Some("2026-02-23"),
            "{after}"
        );
        assert!(ds.get("pbf").is_none(), "{after}");
        assert!(ds.get("osc").is_none(), "{after}");
        assert_eq!(
            ds["snapshot"]["20260223"]["pbf"]["raw"]["seq"].as_integer(),
            Some(4912)
        );

        // The snapshot header renders above its sub-tables.
        let header = after
            .find("[plantasjen.datasets.planet.snapshot.20260223]")
            .expect("snapshot header");
        let first_sub = after
            .find("[plantasjen.datasets.planet.snapshot.20260223.pbf.raw]")
            .unwrap();
        assert!(header < first_sub, "{after}");
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
        let after = run_rotation(
            "rotate_inserts_date",
            before,
            "plantasjen",
            "planet",
            "20260223",
            "2026-02-23",
            "2026-04-11",
        );

        // download_date is inserted into the [planet] block.
        let parsed: toml::Value = toml::from_str(&after).unwrap();
        assert_eq!(
            dataset_of(&parsed, "plantasjen", "planet")["download_date"].as_str(),
            Some("2026-04-11"),
            "{after}"
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
        let after = run_rotation(
            "rotate_unrelated",
            before,
            "plantasjen",
            "planet",
            "20260223",
            "2026-02-23",
            "2026-04-11",
        );

        // Denmark untouched.
        assert!(
            after.contains("[plantasjen.datasets.denmark.pbf.raw]"),
            "denmark pbf.raw should be untouched, got:\n{after}"
        );
        assert!(after.contains("file = \"denmark-raw.osm.pbf\""));

        // Planet rotated.
        assert!(after.contains("[plantasjen.datasets.planet.snapshot.20260223.pbf.raw]"));
    }

    // Values the old hand-rolled appenders spliced in raw now round-trip:
    // a quote or backslash in a filename, and a dotted hostname that used to
    // re-nest the table under a different key.
    #[test]
    fn entries_are_escaped() {
        let after = edit_toml(
            "escaped",
            "project = \"pbfhogg\"\n",
            "host.local",
            "denmark",
            |t| {
                t.set_dataset_origin("Geofabrik").unwrap();
                t.set_pbf("raw", "odd \"name\\.osm.pbf", "abc").unwrap();
                t.set_osc(4705, "d-seq4705.osc.gz", "def").unwrap();
            },
        );
        let parsed: toml::Value = toml::from_str(&after).unwrap();
        let ds = dataset_of(&parsed, "host.local", "denmark");
        assert_eq!(ds["origin"].as_str(), Some("Geofabrik"));
        assert_eq!(ds["pbf"]["raw"]["file"].as_str(), Some("odd \"name\\.osm.pbf"));
        assert_eq!(ds["osc"]["4705"]["xxhash"].as_str(), Some("def"));
    }

    // Comments and unrelated tables survive an edit byte-for-byte.
    #[test]
    fn edits_preserve_comments() {
        let before = "\
# top comment
project = \"pbfhogg\"

# denmark is hand-maintained
[plantasjen.datasets.denmark]
origin = \"Geofabrik\" # trailing
";
        let after = edit_toml("preserve", before, "plantasjen", "denmark", |t| {
            t.set_osc(1, "d-seq1.osc.gz", "abc").unwrap();
        });
        assert!(after.starts_with(before), "{after}");
        assert!(after.contains("[plantasjen.datasets.denmark.osc.1]"), "{after}");
    }

    #[test]
    fn remove_snapshot_drops_header_and_sub_tables_only() {
        let before = "\
[h.datasets.d]
origin = \"x\"

[h.datasets.d.snapshot.a]
download_date = \"2026-01-01\"

[h.datasets.d.snapshot.a.pbf.raw]
file = \"a.osm.pbf\"

[h.datasets.d.snapshot.ab]
download_date = \"2026-01-02\"
";
        let after = edit_toml("remove_snapshot", before, "h", "d", |t| {
            t.remove_snapshot("a").unwrap();
        });
        let parsed: toml::Value = toml::from_str(&after).unwrap();
        let snaps = &dataset_of(&parsed, "h", "d")["snapshot"];
        assert!(snaps.get("a").is_none(), "{after}");
        assert!(snaps.get("ab").is_some(), "{after}");
    }

    // A transaction that is never committed leaves the file untouched.
    #[test]
    fn uncommitted_edits_do_not_write() {
        let before = "project = \"pbfhogg\"\n";
        let dir = toml_dir("uncommitted", before);
        let mut toml = DatasetToml::open(&dir, "h", "d").unwrap();
        toml.set_pbf("raw", "r.osm.pbf", "abc").unwrap();
        drop(toml);
        assert_eq!(std::fs::read_to_string(dir.join("brokkr.toml")).unwrap(), before);
    }

    // A dataset registered in an included file is edited there, and the
    // including brokkr.toml is left byte-identical.
    #[test]
    fn edits_land_in_the_included_file_holding_the_dataset() {
        let root = "project = \"pbfhogg\"\ninclude = [\"shared/hosts.toml\"]\n";
        let dir = toml_dir("edit_included", root);
        std::fs::create_dir_all(dir.join("shared")).unwrap();
        let hosts = "[h.datasets.d]\norigin = \"x\"\n";
        std::fs::write(dir.join("shared/hosts.toml"), hosts).unwrap();

        let mut toml = DatasetToml::open(&dir, "h", "d").unwrap();
        toml.set_pbf("raw", "r.osm.pbf", "abc").unwrap();
        toml.commit().unwrap();

        assert_eq!(std::fs::read_to_string(dir.join("brokkr.toml")).unwrap(), root);
        let after = std::fs::read_to_string(dir.join("shared/hosts.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&after).unwrap();
        assert_eq!(
            dataset_of(&parsed, "h", "d")["pbf"]["raw"]["file"].as_str(),
            Some("r.osm.pbf"),
            "{after}"
        );
    }
}
