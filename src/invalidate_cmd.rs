//! Implementation of the `invalidate` command - hard-delete benchmark results
//! and their sidecar profile data by UUID prefix or commit prefix.
//!
//! Dry-run by default: previews which UUIDs would go, then requires `-f` to
//! actually delete. Sidecar-only runs (dirty/failed, no results.db row) are
//! picked up via `uuids_matching_prefix` / `uuids_matching_commit_prefix` so a
//! single invalidate call cleans both sides.

use std::collections::BTreeSet;
use std::path::Path;

use crate::db::{self, QueryFilter};
use crate::error::DevError;
use crate::output;
use crate::resolve::{results_db_path, sidecar_db_path};

pub(crate) fn cmd_invalidate(
    project_root: &Path,
    uuid_prefix: Option<&str>,
    commit_prefix: Option<&str>,
    force: bool,
) -> Result<(), DevError> {
    let rdb_path = results_db_path(project_root);
    let sdb_path = sidecar_db_path(project_root);

    let rdb = if rdb_path.exists() {
        Some(db::ResultsDb::open(&rdb_path)?)
    } else {
        None
    };
    let sdb = if sdb_path.exists() {
        Some(db::sidecar::SidecarDb::open(&sdb_path)?)
    } else {
        None
    };

    if rdb.is_none() && sdb.is_none() {
        output::result_msg("no results.db or sidecar.db found - nothing to invalidate");
        return Ok(());
    }

    // Collect full target UUIDs. BTreeSet gives stable ordering + dedup.
    let mut targets: BTreeSet<String> = BTreeSet::new();
    let descriptor = match (uuid_prefix, commit_prefix) {
        (Some(u), _) => {
            collect_by_uuid_prefix(u, rdb.as_ref(), sdb.as_ref(), &mut targets)?;
            format!("uuid prefix '{u}'")
        }
        (None, Some(c)) => {
            collect_by_commit_prefix(c, rdb.as_ref(), sdb.as_ref(), &mut targets)?;
            format!("commit prefix '{c}'")
        }
        // Clap's required_unless_present guarantees one of the two is set.
        (None, None) => return Err(DevError::Config(
            "invalidate: supply a UUID prefix or --commit <hash>".into(),
        )),
    };

    if targets.is_empty() {
        output::result_msg(&format!("no matching rows for {descriptor}"));
        return Ok(());
    }

    let target_list: Vec<String> = targets.into_iter().collect();
    preview(&target_list, rdb.as_ref(), sdb.as_ref(), &descriptor);

    if !force {
        output::result_msg("dry-run - re-run with -f / --force to delete");
        return Ok(());
    }

    let mut results_removed = 0usize;
    let mut sidecar_sessions_removed = 0usize;
    for uuid in &target_list {
        if let Some(ref db) = rdb {
            results_removed += db.delete_by_uuid_prefix(uuid)?;
        }
        if let Some(ref db) = sdb {
            sidecar_sessions_removed += db.delete_by_uuid_prefix(uuid)?;
        }
    }
    output::result_msg(&format!(
        "deleted {results_removed} results row(s), {sidecar_sessions_removed} sidecar session(s)",
    ));
    Ok(())
}

fn collect_by_uuid_prefix(
    uuid_prefix: &str,
    rdb: Option<&db::ResultsDb>,
    sdb: Option<&db::sidecar::SidecarDb>,
    targets: &mut BTreeSet<String>,
) -> Result<(), DevError> {
    // Resolve sidecar latest-keys (e.g. "dirty") so the user can invalidate
    // the last dirty/failed run by its alias.
    let resolved = sdb
        .map(|s| s.resolve_latest(uuid_prefix))
        .unwrap_or_else(|| uuid_prefix.to_owned());

    if let Some(db) = rdb {
        for row in db.query_by_uuid(&resolved)? {
            targets.insert(row.uuid);
        }
    }
    if let Some(db) = sdb {
        for uuid in db.uuids_matching_prefix(&resolved)? {
            targets.insert(uuid);
        }
    }
    Ok(())
}

fn collect_by_commit_prefix(
    commit_prefix: &str,
    rdb: Option<&db::ResultsDb>,
    sdb: Option<&db::sidecar::SidecarDb>,
    targets: &mut BTreeSet<String>,
) -> Result<(), DevError> {
    if let Some(db) = rdb {
        // A single commit can produce many runs (especially for A/B sweeps).
        // Pick a generous limit so we don't silently truncate.
        let filter = QueryFilter {
            commit: Some(commit_prefix.to_owned()),
            limit: 100_000,
            ..Default::default()
        };
        for row in db.query(&filter)? {
            if !row.uuid.is_empty() {
                targets.insert(row.uuid);
            }
        }
    }
    if let Some(db) = sdb {
        for uuid in db.uuids_matching_commit_prefix(commit_prefix)? {
            targets.insert(uuid);
        }
    }
    Ok(())
}

fn preview(
    uuids: &[String],
    rdb: Option<&db::ResultsDb>,
    sdb: Option<&db::sidecar::SidecarDb>,
    descriptor: &str,
) {
    output::result_msg(&format!(
        "{} UUID(s) match {descriptor}:",
        uuids.len(),
    ));
    for uuid in uuids {
        let short = &uuid[..8.min(uuid.len())];
        let results_row = rdb.and_then(|db| {
            db.query_by_uuid(uuid)
                .ok()
                .and_then(|mut v| v.pop())
        });
        let sidecar_present = sdb.is_some_and(|db| db.has_data(uuid));
        let detail = match results_row {
            Some(row) => format!(
                "{} {} {} (elapsed {}ms)",
                row.commit.chars().take(8).collect::<String>(),
                row.command,
                if row.mode.is_empty() { "-" } else { row.mode.as_str() },
                row.elapsed_ms,
            ),
            None => String::from("(sidecar-only, no results row)"),
        };
        let sc_tag = if sidecar_present { " [+sidecar]" } else { "" };
        output::result_msg(&format!("  {short}  {detail}{sc_tag}"));
    }
}
