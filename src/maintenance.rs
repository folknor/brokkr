//! Store maintenance: removing what killed brokkr commands left, once per
//! fresh hold, before the acquiring command does any work.
//!
//! Each target is named by construction by the store that makes it, and is
//! made only by a command that holds the brokkr lock: a harness session
//! directory (`check`, `test`), a nextest-lane generated config (`check`), a
//! corpus-sort spill directory (`pmtiles-corpus`), an artifact staging copy
//! (`build`, `run`). A fresh exclusive hold proves that no such command is
//! live, and [`MaintenancePhase`], which only dispatch's own fresh
//! acquisition carries, and only once, is consumed here before the command
//! runs, so nothing this command makes can be swept. Removing a session's path
//! closes nothing a surviving test still holds open.
//!
//! What is out of scope, and why, is in `docs/commands/clean.md`: atomic-write
//! temps, whose writers do not all share one exclusion, and anything a
//! lockless command writes.

use std::path::{Path, PathBuf};

use crate::clean::layout::ProjectState;
use crate::lockfile::MaintenancePhase;
use crate::output;

/// Whether maintenance removes what it finds or only says what it would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Apply,
    /// For an invocation whose contract is to remove nothing (`clean
    /// --dry-run`).
    Report,
}

/// Run every store's maintenance, consuming the phase. `state_root` is the
/// project whose `.brokkr` is maintained; the per-user stores are maintained
/// regardless.
pub(crate) fn run(_phase: MaintenancePhase<'_>, mode: Mode, state_root: Option<&Path>) {
    let found = leftovers(state_root);
    if found.is_empty() {
        return;
    }
    match mode {
        Mode::Report => {
            for path in &found {
                output::dry_run_msg(&format!("would remove leftover {}", output::path(path)));
            }
        }
        Mode::Apply => apply(&found),
    }
}

/// Every leftover the stores name, the per-user ones first.
fn leftovers(state_root: Option<&Path>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Some(harness) = crate::user_dirs::harness_dir() {
        found.extend(children(&harness, crate::test_runner::is_session_dir_name));
    }
    if let Some(root) = state_root {
        let state = ProjectState::new(root);
        for base in [state.nextest_synth_file(), state.nextest_sweep_env_file()] {
            found.extend(children(state.root(), |name| {
                crate::check_cmd::is_generated_config_name(&base, name)
            }));
        }
        found.extend(children(
            &state.corpus_sort_dir(),
            crate::elivagar::corpus::sort::is_spill_dir_name,
        ));
        found.extend(children(
            &state.artifacts_dir(),
            crate::runnables::is_staging_copy_name,
        ));
    }
    found
}

/// The entries of `dir` whose names `matches` accepts; none when `dir` is
/// absent or unreadable.
fn children(dir: &Path, matches: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(&matches))
        .map(|entry| entry.path())
        .collect()
}

/// Remove each leftover, then say what went in one line: the count, and the
/// allocated bytes observed before removal, which is not a claim about space
/// released (another link or open descriptor can keep it). A measurement
/// failure never stops a removal; a removal failure is warned.
fn apply(found: &[PathBuf]) {
    let mut removed = 0usize;
    let mut bytes = 0u64;
    let mut incomplete = false;
    for path in found {
        match crate::health::usage(path) {
            crate::health::Measured::Found(usage) => {
                bytes += usage.bytes;
                incomplete |= usage.incomplete;
            }
            _ => incomplete = true,
        }
        let result = if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            std::fs::remove_dir_all(path) // gate: brokkr's own leftover, under a fresh hold
        } else {
            std::fs::remove_file(path) // gate: brokkr's own leftover, under a fresh hold
        };
        match result {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => output::warn(&format!("leftover {} not removed: {e}", output::path(path))),
        }
    }
    if removed > 0 {
        output::run_msg(&format!(
            "removed {} a killed brokkr left ({}{} allocated observed)",
            output::count(removed, "leftover"),
            if incomplete { "at least " } else { "" },
            output::format_bytes(bytes),
        ));
    }
}

/// `A-B` for two non-empty runs of ASCII digits: the PID and sequence or
/// timestamp tail the stores put in their leftover names.
pub(crate) fn is_number_pair(text: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    text.split_once('-')
        .is_some_and(|(a, b)| digits(a) && digits(b))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Each store recognises exactly the names it makes.
    #[test]
    fn each_store_recognises_only_its_own_leftover_names() {
        use crate::check_cmd::is_generated_config_name as config;
        use crate::elivagar::corpus::sort::is_spill_dir_name as spill;
        use crate::runnables::is_staging_copy_name as staging;
        use crate::test_runner::is_session_dir_name as session;
        let synth = ProjectState::new(Path::new("/s")).nextest_synth_file();
        let synth = synth.as_path();

        assert!(session("4242-1730000000000000000-0"));
        assert!(!session("4242-17300"));
        assert!(!session("lock"));
        assert!(config(synth, "nextest-synth.4242-3.toml"));
        assert!(!config(synth, "nextest-synth.toml"));
        assert!(!config(synth, "nextest-sweep-env.4242-3.toml"));
        assert!(spill("brokkr-corpus-sort-4242-7"));
        assert!(!spill("brokkr-corpus-sort-x-7"));
        assert!(staging(".staging-4242-pbfhogg"));
        assert!(!staging(".staging-x-pbfhogg"));
        assert!(!staging(".staging-4242-"));
        assert!(!staging("0123abcd"));
    }

    /// Only the named leftovers are found; a user's file beside them is not.
    #[test]
    fn leftovers_are_found_by_name_in_their_stores_only() {
        let root = crate::test_scratch::scratch("maintenance", "leftovers");
        let state = ProjectState::new(&root);
        std::fs::create_dir_all(state.corpus_sort_dir().join("brokkr-corpus-sort-9-1")).unwrap();
        std::fs::create_dir_all(state.corpus_sort_dir().join("mine")).unwrap();
        std::fs::create_dir_all(state.artifacts_dir()).unwrap();
        std::fs::write(state.artifacts_dir().join(".staging-9-tool"), b"x").unwrap();
        std::fs::write(state.root().join("nextest-synth.9-2.toml"), b"x").unwrap();
        std::fs::write(state.root().join("notes.toml"), b"x").unwrap();
        let mut found: Vec<PathBuf> = leftovers(Some(&root))
            .into_iter()
            .filter(|p| p.starts_with(&root))
            .collect();
        found.sort();
        let mut expected = vec![
            state.artifacts_dir().join(".staging-9-tool"),
            state.corpus_sort_dir().join("brokkr-corpus-sort-9-1"),
            state.root().join("nextest-synth.9-2.toml"),
        ];
        expected.sort();
        assert_eq!(found, expected);
    }
}
