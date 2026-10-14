//! Per-invocation run-log reservation and retention.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many recent run logs to keep, per kind. Enough that a watchdog-killed
/// run survives the retries that follow it; bounded so the directory cannot
/// grow without limit on a machine that runs `check` all day. Per kind, so a
/// burst of `brokkr test` runs cannot evict the `check` log being investigated.
const RUN_LOGS_KEPT: usize = 10;

/// Retention policy for the human-readable run logs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunLogTunables {
    pub(crate) retained: usize,
    pub(crate) name_attempts: usize,
}

pub(crate) const DEFAULT_RUN_LOG_TUNABLES: RunLogTunables = RunLogTunables {
    retained: RUN_LOGS_KEPT,
    name_attempts: 1_000,
};

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
    open_run_log_with(state_root, kind, &DEFAULT_RUN_LOG_TUNABLES, SystemTime::now);
}

/// Open with explicit policy and clock inputs so naming and retention do not
/// depend on a test machine's wall clock or the production retention budget.
fn open_run_log_with(
    state_root: &Path,
    kind: &str,
    tunables: &RunLogTunables,
    now: impl FnOnce() -> SystemTime,
) {
    let dir = crate::clean::layout::ProjectState::new(state_root).check_logs_dir();
    if let Err(error) = std::fs::create_dir_all(&dir) {
        crate::output::warn(&format!(
            "could not create run-log directory {}: {error}; continuing without a run log",
            dir.display()
        ));
        return;
    }
    let (mut before, retention_ready): (Vec<PathBuf>, bool) = match std::fs::read_dir(&dir) {
        Ok(entries) => (entries.flatten().map(|entry| entry.path()).collect(), true),
        Err(error) => {
            crate::output::warn(&format!(
                "could not list existing run logs in {}: {error}; this run's name order and retention may be inaccurate",
                dir.display()
            ));
            (Vec::new(), false)
        }
    };
    let first_stamp = next_log_stamp(&before, kind, now());
    // The epoch stamp is raised past the greatest existing stamp. That keeps
    // lexicographic retention order chronological when the system clock moves
    // backwards, and create_new protects against another writer at the same
    // time without truncating its log.
    let mut opened = None;
    let mut open_error = None;
    for step in 0..tunables.name_attempts.max(1) {
        let step = u128::try_from(step).unwrap_or(u128::MAX);
        let path = dir.join(format!(
            "{kind}-{:015}.log",
            first_stamp.saturating_add(step)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                opened = Some((path, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                open_error = Some(error);
            }
            Err(error) => {
                open_error = Some(error);
                break;
            }
        }
    }
    if let Some((current, file)) = opened {
        // Prune only after creating it: a failed open must not evict history.
        if retention_ready {
            before.push(current.clone());
            for oldest in logs_to_prune_with(before, kind, &current, tunables.retained) {
                std::fs::remove_file(oldest).ok();
            }
        }
        crate::output::open_run_log(file);
        let argv: Vec<String> = std::env::args().collect();
        crate::output::detail(&format!("argv: {}", argv.join(" ")));
        if let Ok(cwd) = std::env::current_dir() {
            crate::output::detail(&format!("cwd: {}", cwd.display()));
        }
    } else {
        let detail = open_error.map_or_else(
            || "no filename was attempted".to_owned(),
            |error| error.to_string(),
        );
        crate::output::warn(&format!(
            "could not open a unique {kind} run log in {}: {detail}; continuing without a run log",
            dir.display()
        ));
    }
}

/// Choose a millisecond-shaped name that sorts after every existing log of
/// this kind, even when the wall clock has moved backwards.
fn next_log_stamp(existing: &[PathBuf], kind: &str, now: SystemTime) -> u128 {
    let wall_stamp = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let latest = existing
        .iter()
        .filter_map(|path| run_log_stamp(path, kind))
        .max();
    latest.map_or(wall_stamp, |latest| {
        wall_stamp.max(latest.saturating_add(1))
    })
}

fn run_log_stamp(path: &Path, kind: &str) -> Option<u128> {
    if !is_run_log_name(path, kind) {
        return None;
    }
    path.file_name()?
        .to_str()?
        .strip_prefix(kind)?
        .strip_prefix('-')?
        .strip_suffix(".log")?
        .parse()
        .ok()
}

/// Reserve one retention slot for the current file, even after a clock
/// rollback. Other kinds never enter this kind's retention budget.
#[cfg(test)]
fn logs_to_prune(existing: Vec<PathBuf>, kind: &str, current: &Path) -> Vec<PathBuf> {
    logs_to_prune_with(existing, kind, current, DEFAULT_RUN_LOG_TUNABLES.retained)
}

fn logs_to_prune_with(
    mut existing: Vec<PathBuf>,
    kind: &str,
    current: &Path,
    retained: usize,
) -> Vec<PathBuf> {
    existing.retain(|path| is_run_log_name(path, kind) && path != current);
    existing.sort();
    let count = existing.len().saturating_sub(retained.max(1) - 1);
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
