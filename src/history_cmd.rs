//! Implementation of the `history` command and best-effort history recording.

use crate::config;
use crate::env;
use crate::error;
use crate::error::DevError;
use crate::git;
use crate::history;
use crate::project;

pub(crate) struct HistoryQuery {
    pub id: Option<i64>,
    pub command: Option<String>,
    pub project: Option<String>,
    pub project_dir: Option<String>,
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
        project_dir: q.project_dir,
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

/// Best-effort recording of command history. Warns once on failure.
pub(crate) fn record_history(raw_args: &str, elapsed_ms: u64, exit_code: i32) {
    // Sandboxes (and other minimal envs) may have neither XDG_DATA_HOME nor
    // HOME set - there's nowhere to put history.db, and emitting a warning
    // on every invocation is just noise.
    if std::env::var_os("XDG_DATA_HOME").is_none() && std::env::var_os("HOME").is_none() {
        return;
    }

    let inner = || -> Result<(), error::DevError> {
        let db = history::HistoryDb::open()?;

        // Best-effort metadata collection. Each item can fail independently.
        let hostname = config::hostname().unwrap_or_else(|_| "unknown".into());
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".into());

        // Try to detect project and git info - these are optional.
        let (project_name, commit_hash, dirty) = match project::detect() {
            Ok((project, _config, project_root)) => match git::collect(&project_root) {
                Ok(gi) => (
                    Some(project.name().to_owned()),
                    if gi.commit.is_empty() {
                        None
                    } else {
                        Some(gi.commit)
                    },
                    Some(!gi.is_clean),
                ),
                Err(_) => (Some(project.name().to_owned()), None, None),
            },
            Err(_) => (None, None, None),
        };

        let kernel = std::fs::read_to_string("/proc/version")
            .ok()
            .and_then(|s| s.split_whitespace().nth(2).map(String::from));
        let (_, avail) = env::read_memory();
        let avail_memory_mb = i64::try_from(avail).ok();

        #[allow(clippy::cast_possible_wrap)]
        let elapsed = elapsed_ms as i64;

        db.insert(&history::HistoryRow {
            project: project_name,
            cwd,
            command: raw_args.to_owned(),
            elapsed_ms: elapsed,
            exit_status: exit_code,
            hostname,
            commit_hash,
            dirty,
            kernel,
            avail_memory_mb,
        })?;
        Ok(())
    };

    if let Err(e) = inner() {
        eprintln!("[history] warning: failed to write history: {e}");
    }
}
