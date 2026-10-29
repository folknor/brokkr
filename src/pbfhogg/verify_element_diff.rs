//! The element diff two verify checks classify: `pbfhogg diff --format osc`
//! between two outputs, streamed into per-class counts with bounded ID
//! samples.
//!
//! With the first PBF as the diff's old side and the second as its new side,
//! `<delete>` is an element only the first holds, `<create>` one only the
//! second holds, and `<modify>` a key both hold with unequal content
//! (coordinates, tags, refs, members; pbfhogg's content equality, which
//! leaves metadata out). What each class means is the caller's policy:
//! `verify_merge` exempts osmium-only IDs its input deleted, `verify_extract`
//! gates on content and reports membership.

use std::collections::BTreeSet;
use std::path::Path;

use super::verify::{PbfArgv, VerifyHarness};
use crate::error::DevError;
use crate::osc::{self, OscAction, OscKind};
use crate::output::verify_msg;

/// Cap on the IDs kept per kind per class, so a runaway difference does not
/// bury the summary. The counts stay exact.
pub(super) const MAX_REPORTED_IDS: usize = 10;

/// A count of entries and the smallest [`MAX_REPORTED_IDS`] distinct IDs
/// among them.
#[derive(Default)]
pub(super) struct Sampled {
    pub(super) count: u64,
    smallest: BTreeSet<i64>,
}

impl Sampled {
    pub(super) fn add(&mut self, id: i64) {
        self.count += 1;
        self.smallest.insert(id);
        if self.smallest.len() > MAX_REPORTED_IDS {
            self.smallest.pop_last();
        }
    }

    #[cfg(test)]
    pub(super) fn ids(&self) -> Vec<i64> {
        self.smallest.iter().copied().collect()
    }
}

/// One outcome class, per kind (node, way, relation).
#[derive(Default)]
pub(super) struct PerType {
    pub(super) kinds: [Sampled; 3],
}

impl PerType {
    pub(super) fn add(&mut self, kind: OscKind, id: i64) {
        self.kinds[kind.index()].add(id);
    }

    pub(super) fn total(&self) -> u64 {
        self.kinds.iter().map(|k| k.count).sum()
    }

    /// `"N nodes, N ways, N relations"`.
    pub(super) fn counts_line(&self) -> String {
        let [n, w, r] = &self.kinds;
        format!("{} nodes, {} ways, {} relations", n.count, w.count, r.count)
    }

    /// Print `<prefix><label>: <counts>` and the ID samples of every
    /// non-empty kind, through `emit`; nothing when the class is empty.
    pub(super) fn print(&self, prefix: &str, label: &str, emit: fn(&str)) {
        if self.total() == 0 {
            return;
        }
        emit(&format!("  {prefix}{label}: {}", self.counts_line()));
        let [n, w, r] = &self.kinds;
        print_id_sample("nodes", n, emit);
        print_id_sample("ways", w, emit);
        print_id_sample("relations", r, emit);
    }
}

fn print_id_sample(kind: &str, sampled: &Sampled, emit: fn(&str)) {
    if sampled.count == 0 {
        return;
    }
    let shown = sampled.smallest.len() as u64;
    let suffix = if sampled.count > shown {
        format!(" (smallest IDs; {} entries in all)", sampled.count)
    } else {
        String::new()
    };
    let list = sampled
        .smallest
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    emit(&format!("    {kind}: {list}{suffix}"));
}

/// Run `pbfhogg diff --format osc <old> <new> -o <osc>` and hand every entry
/// of the written OSC to `each`, in file order.
///
/// Returns only once the whole document has been scanned, so a caller acts
/// on what it accumulated only after `Ok`. The exit status must agree with
/// what the document holds: 0 with no entries (identical) or 1 with at least
/// one (different). Any other status, a missing OSC, or a disagreement (0
/// with entries, 1 with none) is an error, never a verdict. `osc` must lie in
/// a directory [`VerifyHarness::subdir`] emptied, so its existence proves
/// this run wrote it.
pub(super) fn scan_osc_diff(
    harness: &VerifyHarness,
    old: &Path,
    new: &Path,
    osc_path: &Path,
    scratch: &Path,
    mut each: impl FnMut(OscAction, OscKind, i64),
) -> Result<(), DevError> {
    // No `--suppress-common`: pbfhogg refuses it with `--format osc`, whose
    // document holds only differing elements anyway.
    let captured = harness.run_pbfhogg(
        PbfArgv::new("diff")
            .arg("--format")
            .arg("osc")
            .output(osc_path)?
            .input(old)?
            .input(new)?
            .scratch(scratch),
    )?;
    let stderr = || String::from_utf8_lossy(&captured.stderr).trim().to_owned();
    let differs = match captured.code() {
        Some(0) => false,
        Some(1) => true,
        _ => {
            return Err(DevError::Subprocess {
                program: "pbfhogg diff --format osc".into(),
                ending: captured
                    .ending
                    .process_ending()
                    .unwrap_or(crate::error::ProcessEnding::Code(-1)),
                stderr: stderr(),
            });
        }
    };
    if !osc_path.exists() {
        return Err(DevError::Verify(format!(
            "pbfhogg diff produced no output OSC at {} - stderr: {}",
            osc_path.display(),
            stderr()
        )));
    }
    let mut entries: u64 = 0;
    osc::scan_osc_file(osc_path, |action, kind, id| {
        entries += 1;
        each(action, kind, id);
    })?;
    exit_agrees(differs, entries).map_err(|why| {
        DevError::Verify(format!(
            "pbfhogg diff --format osc: {why} in {} - stderr: {}",
            osc_path.display(),
            stderr()
        ))
    })?;
    verify_msg(&format!(
        "  element diff: {entries} entries in {}",
        osc_path.display()
    ));
    Ok(())
}

/// Whether the diff's exit status agrees with the entries its OSC holds.
fn exit_agrees(differs: bool, entries: u64) -> Result<(), String> {
    match (differs, entries) {
        (false, 0) | (true, 1..) => Ok(()),
        (false, n) => Err(format!("exit 0 (identical) but {n} entries")),
        (true, 0) => Err("exit 1 (different) but no entries".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_must_agree_with_the_entries() {
        assert!(exit_agrees(false, 0).is_ok());
        assert!(exit_agrees(true, 3).is_ok());
        assert!(exit_agrees(false, 1).is_err());
        assert!(exit_agrees(true, 0).is_err());
    }

    #[test]
    fn a_huge_class_keeps_exact_counts_and_bounded_samples() {
        let mut class = PerType::default();
        for id in (0..100_000).rev() {
            class.add(OscKind::Node, id);
        }
        assert_eq!(class.kinds[0].count, 100_000);
        assert_eq!(class.kinds[0].ids(), (0..10).collect::<Vec<i64>>());
        assert_eq!(MAX_REPORTED_IDS, 10);
        assert_eq!(class.total(), 100_000);
    }
}
