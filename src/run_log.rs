//! Per-invocation run-log reservation and retention.

use std::path::{Path, PathBuf};

/// How many recent run logs to keep, per kind. Enough that a watchdog-killed
/// run survives the retries that follow it; bounded so the directory cannot
/// grow without limit on a machine that runs `check` all day. Per kind, so a
/// burst of `brokkr test` runs cannot evict the `check` log being investigated.
const RUN_LOGS_KEPT: usize = 10;

/// The kinds of run log, one name prefix (and one retention budget) each.
pub(crate) mod run_log_kind {
    pub(crate) const CHECK: &str = "check";
    pub(crate) const TEST: &str = "test";
    /// Measured runs (`--bench`/`--hotpath`/`--alloc` and plain runs through
    /// the measurement harness, `sync --bench`).
    pub(crate) const MEASURE: &str = "measure";
    /// `brokkr bench`, the criterion runner.
    pub(crate) const BENCH: &str = "bench";
}

/// Reserved before the lock, activated under it, and dropped after the
/// watchdog joins. Reserving touches no files: waiting commands cannot prune
/// the current holder's log. Nested reservations borrow the outer log.
pub(crate) struct RunLog {
    owner: bool,
}

struct LogReservation {
    root: PathBuf,
    kind: String,
    activated: bool,
}

static LOG_RESERVATION: std::sync::Mutex<Option<LogReservation>> = std::sync::Mutex::new(None);

impl RunLog {
    pub(crate) fn begin(state_root: &Path, kind: &str) -> Self {
        let owner = {
            let mut reservation = LOG_RESERVATION
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if reservation.is_some() {
                false
            } else {
                *reservation = Some(LogReservation {
                    root: state_root.to_owned(),
                    kind: kind.to_owned(),
                    activated: false,
                });
                true
            }
        };
        // A reservation made inside an existing hold needs no fresh acquisition.
        if crate::lockfile::current_hold().is_some() {
            activate_run_log();
        }
        Self { owner }
    }
}

/// Called by the real lock's drain hook, after flock and before any toolchain
/// narration. Re-entrant acquisitions never reopen or prune the log.
pub(crate) fn activate_run_log() {
    let mut reservation = LOG_RESERVATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(log) = reservation.as_mut()
        && !log.activated
    {
        log.activated = true;
        open_run_log(&log.root, &log.kind);
    }
}

impl Drop for RunLog {
    fn drop(&mut self) {
        if self.owner {
            let mut reservation = LOG_RESERVATION
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::output::close_run_log();
            reservation.take();
        }
    }
}

/// Open this run's log under `<state_root>/.brokkr/check-logs/` as
/// `<kind>-<ms>.log`, pruning the oldest of that kind so that at most
/// [`RUN_LOGS_KEPT`] remain. Best-effort: a log that cannot be opened costs the
/// record, never the run.
fn open_run_log(state_root: &Path, kind: &str) {
    let dir = state_root.join(".brokkr").join("check-logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    // `create_new`, stepping the stamp past a name already taken: two runs
    // resolving the same millisecond must not truncate one another's log.
    // Stepping (rather than a suffix) keeps the name all digits, so name
    // order stays age order for retention.
    let opened = (0..1000u128).find_map(|step| {
        let path = dir.join(format!("{kind}-{:015}.log", millis + step));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()
            .map(|file| (path, file))
    });
    if let Some((current, file)) = opened {
        let existing: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| rd.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        // Prune only after creating it: a failed open must not evict history.
        for oldest in logs_to_prune(existing, kind, &current) {
            std::fs::remove_file(oldest).ok();
        }
        crate::output::open_run_log(file);
        let argv: Vec<String> = std::env::args().collect();
        crate::output::detail(&format!("argv: {}", argv.join(" ")));
        if let Ok(cwd) = std::env::current_dir() {
            crate::output::detail(&format!("cwd: {}", cwd.display()));
        }
    }
}

/// Reserve one retention slot for the current file, even after a clock
/// rollback. Other kinds never enter this kind's retention budget.
fn logs_to_prune(mut existing: Vec<PathBuf>, kind: &str, current: &Path) -> Vec<PathBuf> {
    existing.retain(|path| is_run_log_name(path, kind) && path != current);
    existing.sort();
    let count = existing.len().saturating_sub(RUN_LOGS_KEPT - 1);
    existing.into_iter().take(count).collect()
}

/// `<kind>-<digits>.log`, the only name [`open_run_log`] constructs for `kind`
/// - and the only one its pruning may delete.
fn is_run_log_name(path: &Path, kind: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(kind))
        .and_then(|name| name.strip_prefix('-'))
        .and_then(|name| name.strip_suffix(".log"))
        .is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_protects_current_log_and_other_kinds_during_a_burst() {
        // The current clock has rolled back behind every historical stamp.
        let current = PathBuf::from("/x/test-000000000000001.log");
        let mut logs = vec![
            current.clone(),
            PathBuf::from("/x/check-000000000000002.log"),
        ];
        logs.extend((10..40).map(|stamp| PathBuf::from(format!("/x/test-{stamp:015}.log"))));
        let pruned = logs_to_prune(logs.clone(), run_log_kind::TEST, &current);
        assert_eq!(pruned.len(), 21);
        assert_eq!(
            pruned.first(),
            Some(&PathBuf::from("/x/test-000000000000010.log"))
        );
        assert_eq!(
            pruned.last(),
            Some(&PathBuf::from("/x/test-000000000000030.log"))
        );
        assert!(!pruned.contains(&current));
        let kept: Vec<_> = logs
            .into_iter()
            .filter(|path| !pruned.contains(path))
            .collect();
        assert_eq!(
            kept.iter()
                .filter(|path| is_run_log_name(path, run_log_kind::TEST))
                .count(),
            RUN_LOGS_KEPT
        );
        assert!(kept.contains(&PathBuf::from("/x/check-000000000000002.log")));
        assert!(logs_to_prune(kept, run_log_kind::TEST, &current).is_empty());
    }

    #[test]
    fn run_log_names_are_recognised_exactly() {
        let check = run_log_kind::CHECK;
        assert!(is_run_log_name(
            Path::new("/x/check-000001727000000.log"),
            check
        ));
        assert!(!is_run_log_name(Path::new("/x/check-.log"), check));
        assert!(!is_run_log_name(Path::new("/x/check-12a.log"), check));
        assert!(!is_run_log_name(Path::new("/x/notes.log"), check));
        assert!(!is_run_log_name(Path::new("/x/check-12.log.bak"), check));
        // Each kind prunes only its own: a test log is not a check log.
        assert!(!is_run_log_name(
            Path::new("/x/test-000001727000000.log"),
            check
        ));
        assert!(is_run_log_name(
            Path::new("/x/test-000001727000000.log"),
            run_log_kind::TEST
        ));
        assert!(!is_run_log_name(Path::new("/x/checkx-12.log"), check));
    }
}
