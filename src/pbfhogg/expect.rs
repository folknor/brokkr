//! Expected answers: what a pbfhogg answer command should answer on its
//! input, where brokkr.toml declares it.
//!
//! An answer command (`diff`, `diff-snapshots`, `check-refs`, `check-ids`,
//! `check-index`) completes on exit 0 or 1. When no is the bug - a check
//! reporting false violations on an honest file - exit 1 would be stored as
//! a completed run. An input that declares its answer narrows the accepted
//! exits to that one answer, so the contradiction fails the run. Nothing is
//! inferred: an input without a declaration keeps both answers.

use crate::config::{self, CheckVerdict, DigestPair, PbfEntry, PbfExpect};
use crate::datasets::resolve::{self, SnapshotRef};
use crate::error::DevError;
use crate::output;
use crate::output::child::Exits;
use crate::pbfhogg::binary::PbfhoggBinary;
use crate::pbfhogg::commands::PbfhoggCommand;
use crate::pbfhogg::context::CommandParams;
use std::path::Path;

/// One declared answer and the brokkr.toml table that declared it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Expectation {
    /// The exit the answer is: 0 for clean or same, 1 for violations or
    /// differs.
    pub exit: i32,
    pub verdict: &'static str,
    /// Where it was declared, for the messages that name it.
    pub source: String,
}

/// The label of a PBF entry's `expect` table.
pub(crate) fn entry_table(dataset: &str, snapshot: &SnapshotRef, variant: &str) -> String {
    match snapshot {
        SnapshotRef::Base => format!("[<host>.datasets.{dataset}.pbf.{variant}.expect]"),
        SnapshotRef::Named(key) => {
            format!("[<host>.datasets.{dataset}.snapshot.{key}.pbf.{variant}.expect]")
        }
    }
}

/// The registered entry, without resolving or verifying its file: a
/// missing entry is reported by the resolver that needs the file.
fn entry<'a>(
    paths: &'a config::ResolvedPaths,
    dataset: &str,
    snapshot: &SnapshotRef,
    variant: &str,
) -> Option<&'a PbfEntry> {
    let ds = paths.datasets.get(dataset)?;
    match snapshot {
        SnapshotRef::Base => ds.pbf.get(variant),
        SnapshotRef::Named(key) => ds.snapshot.get(key)?.pbf.get(variant),
    }
}

fn check(verdict: Option<CheckVerdict>, table: &str, key: &str) -> Option<Expectation> {
    verdict.map(|v| Expectation {
        exit: v.exit_code(),
        verdict: v.as_str(),
        source: format!("{table} {key}"),
    })
}

/// The answer `expect` declares for `command` on its entry. `osc_seq` is the
/// OSC sequence a `diff` applies; `table` labels the entry's table.
pub(crate) fn for_entry(
    command: &PbfhoggCommand,
    expect: &PbfExpect,
    osc_seq: Option<&str>,
    table: &str,
) -> Option<Expectation> {
    match command {
        PbfhoggCommand::CheckRefs => check(expect.check_refs, table, "check-refs"),
        PbfhoggCommand::CheckIds { .. } => check(expect.check_ids, table, "check-ids"),
        PbfhoggCommand::CheckIndex => check(expect.check_index, table, "check-index"),
        PbfhoggCommand::Diff { .. } => {
            let seq = osc_seq?;
            expect.diff.get(seq).map(|v| Expectation {
                exit: v.exit_code(),
                verdict: v.as_str(),
                source: format!("{table} diff.\"{seq}\""),
            })
        }
        _ => None,
    }
}

/// The answer brokkr.toml declares for a single command's resolved input,
/// or `None` when it declares none.
///
/// `diff-snapshots` looks its pair up by the two entries' registered
/// digests; an entry without one cannot be addressed, so the pair is
/// unknown. A `diff` resolves its OSC sequence as the plan does, only when
/// the entry declares any `diff` answer.
pub(crate) fn resolve(
    command: &PbfhoggCommand,
    dataset: &str,
    variant: &str,
    osc_seq: Option<&str>,
    params: &CommandParams,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<Option<Expectation>, DevError> {
    match command {
        PbfhoggCommand::DiffSnapshots { .. } => {
            let (Some(from), Some(to)) = (&params.from_snapshot, &params.to_snapshot) else {
                return Ok(None);
            };
            let digest = |snapshot: &SnapshotRef| {
                entry(paths, dataset, snapshot, variant).and_then(|e| e.xxhash)
            };
            let (Some(a), Some(b)) = (digest(from), digest(to)) else {
                return Ok(None);
            };
            let pair = DigestPair::new(a, b);
            Ok(paths
                .datasets
                .get(dataset)
                .and_then(|ds| ds.expect.diff_snapshots.get(&pair))
                .map(|v| Expectation {
                    exit: v.exit_code(),
                    verdict: v.as_str(),
                    source: format!("[<host>.datasets.{dataset}.expect.diff-snapshots] \"{pair}\""),
                }))
        }
        PbfhoggCommand::CheckRefs
        | PbfhoggCommand::CheckIds { .. }
        | PbfhoggCommand::CheckIndex
        | PbfhoggCommand::Diff { .. } => {
            let snapshot = SnapshotRef::from_opt(params.snapshot.as_ref());
            let Some(e) = entry(paths, dataset, &snapshot, variant) else {
                return Ok(None);
            };
            let seq = if matches!(command, PbfhoggCommand::Diff { .. }) && !e.expect.diff.is_empty()
            {
                Some(
                    resolve::resolve_single_osc(dataset, &snapshot, osc_seq, paths, project_root)?
                        .1,
                )
            } else {
                None
            };
            Ok(for_entry(
                command,
                &e.expect,
                seq.as_deref(),
                &entry_table(dataset, &snapshot, variant),
            ))
        }
        _ => Ok(None),
    }
}

/// The exits a run of `command` on `binary` stores as completed.
///
/// A command that gives no answer completes on 0. An answer command
/// completes on the answers this build can be trusted to give - 0 and 1, or
/// 0 alone on a build from before pbfhogg's split of answers from failures
/// (854f9465), whose exit 1 may be a crash - narrowed to the declared
/// answer when `expectation` names one. A declared answer the build cannot
/// be trusted to give leaves nothing to accept, and the run is refused
/// before it starts.
pub(crate) fn completed_exits(
    command: &PbfhoggCommand,
    binary: &PbfhoggBinary,
    expectation: Option<&Expectation>,
) -> Result<Exits, DevError> {
    if command.ok_exit_codes().is_empty() {
        return Ok(Exits::SUCCESS);
    }
    let split = binary.splits_answer_from_failure()?;
    if !split {
        output::warn(&format!(
            "this pbfhogg build predates the exit-code split (pbfhogg 854f9465): its exit 1 is \
             either a negative answer or a failure, so a `{}` run that exits 1 fails instead of \
             being stored",
            command.id()
        ));
    }
    let trusted: &'static [i32] = if split { &[0, 1] } else { &[0] };
    let Some(expectation) = expectation else {
        return Ok(Exits::answer(trusted));
    };
    if !trusted.contains(&expectation.exit) {
        return Err(DevError::Config(format!(
            "`{}` is expected to answer {} on this input ({}), and this pbfhogg build predates \
             the exit-code split, so that answer cannot be told from a failure; the run could \
             not pass and was not started",
            command.id(),
            expectation.verdict,
            expectation.source,
        )));
    }
    output::bench_msg(&format!(
        "{} expects {} ({}): the other answer fails the run",
        command.id(),
        expectation.verdict,
        expectation.source,
    ));
    Ok(Exits::answer(if expectation.exit == 0 {
        &[0]
    } else {
        &[1]
    }))
}

/// Restate a run that gave the other answer as a contradiction of its
/// declaration, naming both answers and where it was declared. Any other
/// failure, and any failure of an undeclared run, is returned unchanged.
pub(crate) fn explain(
    command: &PbfhoggCommand,
    expectation: Option<&Expectation>,
    error: DevError,
) -> DevError {
    let Some(expectation) = expectation else {
        return error;
    };
    let code = match &error {
        DevError::Subprocess {
            ending: crate::error::ProcessEnding::Code(code),
            ..
        } => *code,
        DevError::ExitCode(code) => *code,
        _ => return error,
    };
    if !matches!(code, 0 | 1) || code == expectation.exit {
        return error;
    }
    DevError::Config(format!(
        "`{}` exited {code}, but {} declares it should answer {} (exit {}); the run is \
         failed, not stored",
        command.id(),
        expectation.source,
        expectation.verdict,
        expectation.exit,
    ))
}

/// The exits a dry run reports: the command's own, with no build to ask.
pub(crate) fn dry_run_exits(command: &PbfhoggCommand) -> Exits {
    if command.ok_exit_codes().is_empty() {
        Exits::SUCCESS
    } else {
        Exits::answer(&[0, 1])
    }
}

#[cfg(test)]
#[path = "expect_tests.rs"]
mod tests;
