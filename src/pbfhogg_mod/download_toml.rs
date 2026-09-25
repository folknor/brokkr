// `brokkr.toml` edits for dataset management: `download`, `download
// --refresh`, `download --as-snapshot`, and `--as-snapshot` promotion of a
// repack/degrade artefact.
//
// Every edit goes through one `DatasetToml` transaction: parse the file into a
// `toml_edit` document, apply the whole batch of changes in memory, and commit
// once with an atomic replace (sibling temp file, fsync, rename, directory
// fsync). Two properties follow, and both were missing from the hand-rolled
// appenders this replaced:
//
// - **Escaping.** Keys and values are written by `toml_edit`, so a hostname,
//   dataset key or filename containing `"`, `\` or `.` produces valid TOML
//   instead of a broken or silently re-nested table.
// - **All or nothing.** Each old appender was its own read/push/write, so a
//   flow that wrote five blocks could fail after two, and `--refresh` could
//   rotate the primary out and then die before registering the new one - a
//   dataset with no primary. A flow now builds every change first and commits
//   them together, after every file they reference exists and is hashed; a
//   failure anywhere before the commit leaves `brokkr.toml` byte-identical.
//
// Comments and formatting of everything the edit does not touch survive the
// round trip (`toml_edit` is lossless), so hand-maintained files are safe.

/// One read-modify-write transaction on `<project_root>/brokkr.toml`.
struct DatasetToml {
    path: PathBuf,
    doc: toml_edit::DocumentMut,
    hostname: String,
    dataset_key: String,
    /// One narration line per change, printed only once the commit lands so
    /// the log never claims an edit that was rolled back.
    changes: Vec<String>,
}

impl DatasetToml {
    /// Parse `brokkr.toml` for editing `[<hostname>.datasets.<dataset_key>]`.
    fn open(project_root: &Path, hostname: &str, dataset_key: &str) -> Result<Self, DevError> {
        let path = project_root.join("brokkr.toml");
        let text = std::fs::read_to_string(&path)?;
        let doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| DevError::Config(format!("{}: {e}", path.display())))?;
        Ok(Self {
            path,
            doc,
            hostname: hostname.to_owned(),
            dataset_key: dataset_key.to_owned(),
            changes: Vec::new(),
        })
    }

    /// Display form of a table path under this dataset, for narration.
    fn label(&self, rest: &[&str]) -> String {
        let mut parts = vec![self.hostname.as_str(), "datasets", self.dataset_key.as_str()];
        parts.extend_from_slice(rest);
        format!("[{}]", parts.join("."))
    }

    /// The table at `<host>.datasets.<dataset>.<rest...>`, creating any
    /// missing level as an implicit table (no header of its own).
    fn table_mut(&mut self, rest: &[&str]) -> Result<&mut toml_edit::Table, DevError> {
        let mut path: Vec<&str> = vec![self.hostname.as_str(), "datasets", self.dataset_key.as_str()];
        path.extend_from_slice(rest);
        let mut table = self.doc.as_table_mut();
        for (depth, key) in path.iter().enumerate() {
            if !table.contains_key(key) {
                let mut fresh = toml_edit::Table::new();
                fresh.set_implicit(true);
                table.insert(key, toml_edit::Item::Table(fresh));
            }
            table = table
                .get_mut(key)
                .and_then(toml_edit::Item::as_table_mut)
                .ok_or_else(|| {
                    DevError::Config(format!(
                        "brokkr.toml: [{}] is not a table (inline tables and dotted keys \
                         are not supported here)",
                        path[..=depth].join(".")
                    ))
                })?;
        }
        Ok(table)
    }

    /// Whether `[<host>.datasets.<dataset>.<rest...>]` exists.
    fn has(&self, rest: &[&str]) -> bool {
        let mut item = self.doc.as_item();
        let mut path: Vec<&str> = vec![self.hostname.as_str(), "datasets", self.dataset_key.as_str()];
        path.extend_from_slice(rest);
        for key in path {
            match item.get(key) {
                Some(next) => item = next,
                None => return false,
            }
        }
        true
    }

    /// Make the dataset table explicit and set its `origin`.
    fn set_dataset_origin(&mut self, origin: &str) -> Result<(), DevError> {
        let label = self.label(&[]);
        let ds = self.table_mut(&[])?;
        ds.set_implicit(false);
        ds.insert("origin", toml_edit::value(origin));
        self.changes.push(format!("added {label}"));
        Ok(())
    }

    /// Insert (or replace) the leaf entry `<rest...>.<key>` as an explicit
    /// `file` + `xxhash` table.
    fn set_file_entry(
        &mut self,
        rest: &[&str],
        key: &str,
        filename: &str,
        xxhash: &str,
    ) -> Result<(), DevError> {
        let mut full = rest.to_vec();
        full.push(key);
        let label = self.label(&full);
        let parent = self.table_mut(rest)?;
        let mut entry = toml_edit::Table::new();
        entry.decor_mut().set_prefix("\n");
        entry.insert("file", toml_edit::value(filename));
        entry.insert("xxhash", toml_edit::value(xxhash));
        parent.insert(key, toml_edit::Item::Table(entry));
        self.changes.push(format!("added {label}"));
        Ok(())
    }

    /// `[..pbf.<variant>]` on the primary snapshot.
    fn set_pbf(&mut self, variant: &str, filename: &str, xxhash: &str) -> Result<(), DevError> {
        self.set_file_entry(&["pbf"], variant, filename, xxhash)
    }

    /// `[..osc.<seq>]` on the primary snapshot.
    fn set_osc(&mut self, seq: u64, filename: &str, xxhash: &str) -> Result<(), DevError> {
        self.set_file_entry(&["osc"], &seq.to_string(), filename, xxhash)
    }

    /// `[..snapshot.<key>]` with its `download_date`.
    fn set_snapshot_header(&mut self, snap_key: &str, date: &str) -> Result<(), DevError> {
        let label = self.label(&["snapshot", snap_key]);
        let snap = self.table_mut(&["snapshot", snap_key])?;
        snap.set_implicit(false);
        snap.insert("download_date", toml_edit::value(date));
        if snap.decor().prefix().is_none() {
            snap.decor_mut().set_prefix("\n");
        }
        self.changes.push(format!("added {label}"));
        Ok(())
    }

    /// `[..snapshot.<key>.pbf.<variant>]`.
    fn set_snapshot_pbf(
        &mut self,
        snap_key: &str,
        variant: &str,
        filename: &str,
        xxhash: &str,
    ) -> Result<(), DevError> {
        self.set_file_entry(&["snapshot", snap_key, "pbf"], variant, filename, xxhash)
    }

    /// `[..snapshot.<key>.osc.<seq>]`.
    fn set_snapshot_osc(
        &mut self,
        snap_key: &str,
        seq: u64,
        filename: &str,
        xxhash: &str,
    ) -> Result<(), DevError> {
        self.set_file_entry(&["snapshot", snap_key, "osc"], &seq.to_string(), filename, xxhash)
    }

    /// Drop `[..snapshot.<key>]` and every sub-table under it.
    fn remove_snapshot(&mut self, snap_key: &str) -> Result<(), DevError> {
        let label = self.label(&["snapshot", snap_key]);
        if !self.has(&["snapshot", snap_key]) {
            return Ok(());
        }
        let snapshots = self.table_mut(&["snapshot"])?;
        snapshots.remove(snap_key);
        self.changes.push(format!("removed previous {label} and its sub-tables"));
        Ok(())
    }

    /// Move the dataset's primary `pbf`/`osc` tables under
    /// `snapshot.<snap_key>`, give that snapshot `old_download_date`, and set
    /// the dataset's own `download_date` to `new_download_date`.
    ///
    /// Moved tables keep their bodies (`seq = N` and any comments included)
    /// and their place in the file; only their header changes. The snapshot
    /// header is pinned just ahead of the first moved table so it renders
    /// above its sub-tables rather than wherever a fresh table would land.
    fn rotate_primary_to_snapshot(
        &mut self,
        snap_key: &str,
        old_download_date: &str,
        new_download_date: &str,
    ) -> Result<(), DevError> {
        let label = self.label(&[]);
        let ds = self.table_mut(&[])?;
        let pbf = ds.remove("pbf");
        let osc = ds.remove("osc");
        ds.set_implicit(false);
        ds.insert("download_date", toml_edit::value(new_download_date));

        let first_position = [pbf.as_ref(), osc.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(min_table_position)
            .min();

        self.set_snapshot_header(snap_key, old_download_date)?;
        let snap = self.table_mut(&["snapshot", snap_key])?;
        if let Some(pos) = first_position {
            snap.set_position(Some(pos));
        }
        if let Some(item) = pbf {
            snap.insert("pbf", item);
        }
        if let Some(item) = osc {
            snap.insert("osc", item);
        }
        self.changes
            .push(format!("rotated {label} pbf/osc tables -> snapshot.{snap_key}"));
        Ok(())
    }

    /// Atomically replace `brokkr.toml` with the edited document, then narrate
    /// what changed.
    fn commit(self) -> Result<(), DevError> {
        if self.changes.is_empty() {
            return Ok(());
        }
        crate::atomic_write::replace(&self.path, self.doc.to_string().as_bytes())?;
        for change in &self.changes {
            output::download_msg(&format!("  {change} in brokkr.toml"));
        }
        Ok(())
    }
}

/// The smallest render position of any table at or under `item`.
fn min_table_position(item: &toml_edit::Item) -> Option<isize> {
    let table = item.as_table()?;
    let own = table.position();
    let children = table.iter().filter_map(|(_, child)| min_table_position(child)).min();
    match (own, children) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Move `src` to `dest` without ever exposing a partial `dest`.
///
/// `rename` when both sit on one filesystem. Across filesystems (EXDEV) the
/// copy goes to a sibling temp name first and is renamed into place, so an
/// interrupted copy leaves a stray temp file rather than a truncated `dest`
/// that a later run would hash and register. The raw OS code is matched so
/// this does not depend on `ErrorKind::CrossesDevices`.
fn move_file_into_place(src: &Path, dest: &Path) -> Result<(), DevError> {
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let tmp = dest.with_extension("partial");
            if let Err(e) = std::fs::copy(src, &tmp) {
                std::fs::remove_file(&tmp).ok();
                return Err(DevError::Io(e));
            }
            if let Err(e) = std::fs::rename(&tmp, dest) {
                std::fs::remove_file(&tmp).ok();
                return Err(DevError::Io(e));
            }
            std::fs::remove_file(src).ok();
            Ok(())
        }
        Err(e) => Err(DevError::Io(e)),
    }
}

/// Generate an indexed PBF from `input` with `pbfhogg cat <input> -o <dest>`.
///
/// The one generator for every download flow (primary, `--as-snapshot`,
/// `--refresh`); they used to carry three copies and one had drifted to
/// `--type node,way,relation`. Plain `cat` is the form pbfhogg documents for
/// indexdata generation. Writes to a temp name and renames on success, so a
/// failed or killed `cat` never leaves a truncated `dest` behind.
fn generate_indexed_pbf(
    input: &Path,
    dest: &Path,
    project_root: &Path,
    build_root: &Path,
) -> Result<(), DevError> {
    output::download_msg("  generating indexed PBF via cat");
    let binary = build::cargo_build(
        &build::BuildConfig::release(Some("pbfhogg-cli")),
        build_root,
    )?;
    let binary_str = binary.display().to_string();
    let input_str = input.display().to_string();
    let tmp = dest.with_extension("tmp");
    let tmp_str = tmp.display().to_string();

    let captured = output::run_captured(
        &binary_str,
        &["cat", &input_str, "-o", &tmp_str],
        project_root,
    )?;
    if let Err(e) = captured.check_success(&binary_str) {
        std::fs::remove_file(&tmp).ok();
        return Err(e);
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}
