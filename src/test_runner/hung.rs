//! Hang attribution, process snapshots, and user-facing timeout reports.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{Attribution, HungTest, Leader, TimeoutReason, direct_child_pids};
use crate::process::snapshot_proc;

pub(super) fn capture_hung_test(
    state_root: &Path,
    leader_pid: u32,
    leader: Leader,
    reason: TimeoutReason,
    attribution: Attribution,
    elapsed: Duration,
    ceiling: Duration,
) -> HungTest {
    let component = attribution
        .test_name()
        .map(str::to_owned)
        .unwrap_or_else(|| match &reason {
            TimeoutReason::Wall { .. } => "wall".to_owned(),
            TimeoutReason::NoCompletion => "no-completion".to_owned(),
            TimeoutReason::Idle => "idle".to_owned(),
            TimeoutReason::Watched => "watched".to_owned(),
            TimeoutReason::OneTestWall => "one-test-wall".to_owned(),
        });
    let test_pids = direct_child_pids(leader_pid);
    let snapshot_pid = match leader {
        Leader::Cargo => test_pids.first().copied().or(Some(leader_pid)),
        Leader::TestBinary => Some(leader_pid),
    };
    // Diagnostic snapshots are brokkr-owned state, so they live under the
    // config-dir `project_root` (`state_root`), not the code tree cargo runs
    // in - the two differ when brokkr.toml is one level above a foreign
    // checkout, and the foreign repo must stay clean.
    static SNAPSHOTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _snapshots = SNAPSHOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let snapshots = crate::clean::layout::ProjectState::new(state_root).test_hung_dir();
    prune_snapshots(&snapshots);
    let snapshot_dir = snapshots.join(format!(
        "{}-{}-{}",
        unix_secs(),
        leader_pid,
        sanitize_path_component(&component)
    ));

    let mut snapshot_error = None;
    if let Err(err) = fs::create_dir_all(&snapshot_dir) {
        snapshot_error = Some(err.to_string());
    } else if let Some(pid) = snapshot_pid
        && let Err(err) = snapshot_proc(pid, &snapshot_dir)
    {
        snapshot_error = Some(err.to_string());
    }

    let wchan = snapshot_error
        .is_none()
        .then(|| first_line(snapshot_dir.join("proc-wchan.txt")))
        .flatten();
    let stack = snapshot_error
        .is_none()
        .then(|| first_line(snapshot_dir.join("proc-stack.txt")))
        .flatten();

    HungTest {
        reason,
        attribution,
        leader,
        elapsed,
        ceiling,
        snapshot_dir,
        leader_pid,
        test_pids,
        snapshot_pid,
        wchan,
        stack,
        snapshot_error,
    }
}

/// Diagnostics are a bounded recent history, rather than an archive. Keep
/// room for the snapshot about to be written. Never follow directory symlinks.
fn prune_snapshots(root: &Path) {
    const RETAINED: usize = 16;
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut directories: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            Some((metadata.modified().unwrap_or(UNIX_EPOCH), entry.path()))
        })
        .collect();
    directories.sort_by(|a, b| b.cmp(a));
    for (_, path) in directories.into_iter().skip(RETAINED - 1) {
        drop(fs::remove_dir_all(path));
    }
}

fn first_line(path: PathBuf) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_owned())
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(super) fn sanitize_path_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(120));
    let mut prev_sep = false;
    for ch in name.chars() {
        let keep = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_');
        if keep {
            out.push(ch);
            prev_sep = false;
        } else if !prev_sep {
            out.push('_');
            prev_sep = true;
        }
        if out.len() >= 120 {
            break;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "unknown".into()
    } else {
        trimmed.to_owned()
    }
}

/// True for libtest's standalone status lines (`ok`, `FAILED`, `ignored`,
/// optionally followed by ` <X.Xs>` when `--report-time` is enabled).
/// Also used by `test_cmd`'s display condenser: under `--nocapture` the
/// verdict lands on its own line after the test's output, so the
/// full-line `test NAME ... FAILED` filter never sees it.
pub(crate) fn is_bare_status_line(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(head) = parts.next() else {
        return false;
    };
    if !matches!(head, "ok" | "FAILED" | "ignored") {
        return false;
    }
    // Allow at most one trailing token, and only if it looks like a
    // libtest timing suffix: `<0.001s>`.
    match parts.next() {
        None => true,
        Some(tail) => parts.next().is_none() && tail.starts_with('<') && tail.ends_with("s>"),
    }
}

/// The line naming where the hang snapshot went, or why there is none.
fn snapshot_line(hung: &HungTest, cwd: &Path) -> String {
    match &hung.snapshot_error {
        Some(err) => format!("snapshot failed: {err}"),
        None => {
            let path = hung
                .snapshot_dir
                .strip_prefix(cwd)
                .unwrap_or(&hung.snapshot_dir);
            format!("full snapshot: {}", path.display())
        }
    }
}

fn idle_cause(leader: Leader) -> &'static str {
    match leader {
        Leader::Cargo => {
            "cargo produced no test progress before the first test or after the last; a build-directory lock held by another cargo can cause this"
        }
        Leader::TestBinary => {
            "the directly run test binary produced no test progress before the first test or after the last"
        }
    }
}

fn wall_context(blamed: &[String]) -> String {
    match blamed {
        [] => "no test was in flight".to_owned(),
        names => format!(
            "in flight when it expired (unverified): {}",
            names.join(", ")
        ),
    }
}

fn child_text(pids: &[u32]) -> String {
    match pids {
        [] => "no test child found".to_owned(),
        [pid] => format!("test child (pid {pid})"),
        pids => format!(
            "test children (pids {})",
            pids.iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

pub(crate) fn format_hung_test(hung: &HungTest, cwd: &Path) -> String {
    let child_text = child_text(&hung.test_pids);
    let proc_pid = hung
        .snapshot_pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "?".into());
    // The measured time and the limit are one number when the kill is on time
    // (the usual case), so say it once; both appear only when they differ.
    let (ran, limit) = (hung.elapsed.as_secs(), hung.ceiling.as_secs());
    let timing = |what: &str| {
        if ran == limit {
            format!("exceeded the {limit}s {what}")
        } else {
            format!("ran {ran}s, past the {limit}s {what}")
        }
    };
    let named = |name: &str| match &hung.attribution {
        Attribution::Exact { .. } => format!("test {name}"),
        Attribution::Suspect { stale: true, .. } => format!("test {name} (best guess)"),
        Attribution::Suspect { .. } => format!("test {name} (suspect)"),
        Attribution::None => "an unnamed test".to_owned(),
    };

    let headline = match (&hung.reason, &hung.attribution) {
        (TimeoutReason::Wall { blamed }, Attribution::None) => {
            // Active when a wall deadline expired is context, not cause.
            let context = wall_context(blamed);
            let run = if ran == limit {
                format!("the run exceeded its {limit}s wall deadline")
            } else {
                format!("the run ran {ran}s, past its {limit}s wall deadline")
            };
            format!("{run} and was killed\n  {context}")
        }
        (TimeoutReason::Idle, _) => {
            let cause = idle_cause(hung.leader);
            format!(
                "no test in flight for {ran}s ({}) - {cause}",
                if ran == limit {
                    "the idle ceiling".to_owned()
                } else {
                    format!("past the {limit}s idle ceiling")
                },
            )
        }
        (TimeoutReason::NoCompletion, Attribution::Exact { name }) => {
            if hung.leader == Leader::TestBinary {
                format!(
                    "the direct test binary running `{name}` {} with no completion record observed",
                    timing("process ceiling"),
                )
            } else {
                format!(
                    "{} {} with no completion record observed",
                    named(name),
                    timing("per-test timeout"),
                )
            }
        }
        (TimeoutReason::NoCompletion, _) => format!(
            "no test completed for {ran}s ({}) - the test in progress could not be named",
            if ran == limit {
                "the per-test ceiling".to_owned()
            } else {
                format!("past the {limit}s per-test ceiling")
            },
        ),
        (TimeoutReason::Watched | TimeoutReason::OneTestWall, Attribution::Exact { name }) => {
            if hung.leader == Leader::TestBinary {
                format!(
                    "the direct test binary running `{name}` {}",
                    timing("process ceiling")
                )
            } else {
                format!("{} {}", named(name), timing("per-test timeout"))
            }
        }
        (
            TimeoutReason::Watched | TimeoutReason::OneTestWall,
            Attribution::Suspect { name, .. },
        ) => format!("{} {}", named(name), timing("per-test timeout")),
        (TimeoutReason::Watched | TimeoutReason::OneTestWall, Attribution::None) => {
            format!("an unnamed test {}", timing("per-test timeout"))
        }
        (TimeoutReason::Wall { .. }, _) => "the run wall deadline expired".to_owned(),
    };
    let group = match hung.leader {
        Leader::Cargo => "cargo process group",
        Leader::TestBinary => "the direct test binary's process group",
    };
    let mut out = format!(
        "{headline}\n  killed {group} (pgid {}) and {child_text}",
        hung.leader_pid,
    );
    // Only what was actually read: /proc/<pid>/stack in particular is
    // unreadable for a non-root brokkr, and a line saying so is noise.
    if let Some(wchan) = &hung.wchan {
        out.push_str(&format!("\n  /proc/{proc_pid}/wchan: {wchan}"));
    }
    if let Some(stack) = &hung.stack {
        out.push_str(&format!("\n  /proc/{proc_pid}/stack: {stack}"));
    }
    out.push_str(&format!("\n  {}", snapshot_line(hung, cwd)));
    out
}

#[cfg(test)]
mod retention_tests {
    #![allow(clippy::unwrap_used)]
    #[test]
    fn snapshot_retention_leaves_room_for_the_next_diagnostic() {
        let root = crate::test_scratch::scratch("test_runner_hung", "retention");
        for n in 0..30 {
            std::fs::create_dir(root.join(format!("snapshot-{n}"))).unwrap();
        }
        let preserved = root.join("other-file");
        std::fs::write(&preserved, "untouched").unwrap();
        super::prune_snapshots(&root);
        assert_eq!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter(|e| e.as_ref().unwrap().file_type().unwrap().is_dir())
                .count(),
            15
        );
        assert!(preserved.exists());
    }
}
