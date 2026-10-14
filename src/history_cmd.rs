//! Implementation of the `history` command and best-effort history recording.

use crate::error::DevError;
use crate::git;
use crate::history;
use crate::host;
use crate::project;

pub(crate) struct HistoryQuery {
    pub id: Option<i64>,
    pub command: Option<String>,
    pub project: Option<String>,
    pub failed: bool,
    pub status: Option<i32>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub slow: Option<i64>,
    pub limit: usize,
    pub all: bool,
}

pub(crate) fn cmd_history(q: HistoryQuery) -> Result<(), DevError> {
    let db = history::HistoryDb::open()?;
    if let Some(id) = q.id {
        match db.get_by_id(id)? {
            Some(entry) => println!("{}", history::format_detail(&entry)),
            None => crate::output::result_msg(&format!("no history entry with id {id}")),
        }
        return Ok(());
    }
    let filter = history::HistoryFilter {
        command: q.command,
        project: q.project,
        failed: q.failed,
        status: q.status,
        since: q.since,
        until: q.until,
        slow_ms: q.slow,
        limit: q.limit,
        all: q.all,
    };
    let entries = db.query(&filter)?;
    let output = history::format_history(&entries);
    println!("{output}");
    Ok(())
}

/// Best-effort recording of command history. Arguments are redacted before
/// durable storage and kept as a JSON argv so boundaries survive spaces.
pub(crate) fn record_history(argv: &[String], elapsed_ms: u64, exit_code: i32) {
    // Sandboxes (and other minimal envs) may have neither XDG_DATA_HOME nor
    // HOME set - there's nowhere to put history.db, and emitting a warning
    // on every invocation is just noise.
    if crate::user_dirs::xdg_data_home().is_none() {
        return;
    }

    let inner = || -> Result<(), history::StoreError> {
        let db = history::HistoryDb::open_typed()?;
        let command = serde_json::to_string(&history::redact_argv(argv)).map_err(|error| {
            history::StoreError::Other(DevError::Internal(format!(
                "cannot encode history argv: {error}"
            )))
        })?;

        // Try to detect project and git info - these are optional.
        let (project_name, commit_hash, dirty) = match project::detect() {
            // Git info describes the code under test, so collect it from the
            // build root (cwd), not the config dir.
            Ok(d) => match git::collect(d.build_root.as_path()) {
                Ok(gi) => (
                    Some(d.project.name().to_owned()),
                    Some(gi.commit),
                    Some(!gi.is_clean),
                ),
                Err(_) => (Some(d.project.name().to_owned()), None, None),
            },
            Err(_) => (None, None, None),
        };

        let kernel = host::kernel::kernel_version();
        let memory = host::memory::collect();
        let avail_memory_mb = i64::try_from(memory.available_mb).ok();

        #[allow(clippy::cast_possible_wrap)]
        let elapsed = elapsed_ms as i64;

        db.insert(&history::HistoryRow {
            project: project_name,
            command,
            elapsed_ms: elapsed,
            exit_status: exit_code,
            commit_hash,
            dirty,
            kernel,
            avail_memory_mb,
        })?;
        Ok(())
    };

    // A store this environment cannot write (a sandbox whose data dir is
    // read-only) would warn on every invocation for nothing anyone can act
    // on, so it is skipped silently; anything else is a real fault and says
    // so. `brokkr history` itself still reports every open failure.
    match inner() {
        Ok(()) => {}
        Err(e) if e.is_environmental() => {}
        // Keep late bookkeeping off stdout, which may end in a JSON trailer.
        Err(e) => crate::output::warn_stderr(&format!("history: failed to write history: {e}")),
    }
}
