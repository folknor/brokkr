//! Output-regression diff wrapper.
//!
//! Builds the elivagar release binary and runs
//! `elivagar regress <current> --against <blessed> [flags]`, streaming its
//! report live and propagating its exit code: elivagar `regress` uses exit 0
//! for "no accountable diff" and exit 1 for any set/feature/attr/structural
//! diff (or a tolerance budget overrun), and `brokkr regress` is a gate, so
//! that code must reach the caller unchanged. The current archive comes from
//! the durable output dir (by --commit/--file); the blessed archive is
//! resolved + xxhash-verified from brokkr.toml (or passed via --against).
//!
//! Before any of that, both archives' provenance blocks are compared and an
//! incomparable pair is **refused** - see `provenance.rs` and step 1 of the
//! consumer contract in elivagar's `reference/metadata.md`. The refusal exits
//! `REFUSED` (2), which is neither of elivagar's two codes on purpose: a
//! refusal is not a pass, and reporting it as 1 would recreate the exact
//! false alarm the gate exists to prevent - on 2026-07-14 an incomparable
//! diff was read as a code regression and investigated as one, at length, in
//! the wrong subsystem.
//!
//! The gate runs before `cargo_build` because it is two header reads and a
//! JSON parse, and there is no reason to pay for a compile to be told the
//! comparison was never going to mean anything.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::lockfile::LockGuard;
use crate::output;

use super::provenance::{self, Provenance};

/// Exit code for a refused comparison: the archives are not comparable, which
/// is neither "no accountable diff" (0) nor "regression" (1).
pub const REFUSED: i32 = 2;

#[allow(clippy::too_many_arguments)]
pub fn run(
    current: &Path,
    blessed: &Path,
    project_root: &Path,
    tol: i32,
    max_moved: u64,
    max_examples: usize,
    svg_dump: Option<&Path>,
    json: bool,
    lock: Option<&LockGuard>,
) -> Result<(), DevError> {
    gate_contract(current, blessed)?;

    let binary = build::cargo_build(&build::BuildConfig::release(None), project_root)?;
    let binary_str = binary.display().to_string();

    let current_str = current.display().to_string();
    let blessed_str = blessed.display().to_string();
    let tol_str = tol.to_string();
    let max_moved_str = max_moved.to_string();
    let max_examples_str = max_examples.to_string();
    let svg_dump_str = svg_dump.map(|p| p.display().to_string());

    let mut args: Vec<&str> = vec![
        "regress",
        &current_str,
        "--against",
        &blessed_str,
        "--tol",
        &tol_str,
        "--max-moved",
        &max_moved_str,
        "--max-examples",
        &max_examples_str,
    ];
    if let Some(dir) = &svg_dump_str {
        args.push("--svg-dump");
        args.push(dir);
    }
    if json {
        args.push("--json");
    }

    output::run_msg(&format!("{binary_str} {}", args.join(" ")));

    let out = output::run_passthrough_timed(&binary_str, &args, lock)?;
    if out.code != 0 {
        // Propagate the gate verdict verbatim (the report went to stdout).
        return Err(DevError::ExitCode(out.code));
    }
    Ok(())
}

/// Step 1 of the consumer contract: compare `input` and `config`, and refuse
/// the geometry comparison on mismatch rather than emitting a diff.
///
/// An archive with no block cannot be gated. Saying so - and stopping - is the
/// contract's own instruction, and the alternative is assuming comparability,
/// which is the assumption that produced 363,620 phantom structural moves.
/// There is deliberately no override flag: an escape hatch would just be the
/// prose rule again, with a flag.
fn gate_contract(current: &Path, blessed: &Path) -> Result<(), DevError> {
    let cur = read_gateable(current, "current")?;
    let bless = read_gateable(blessed, "blessed")?;

    let mismatches = provenance::contract_mismatches(&cur, &bless);
    if !mismatches.is_empty() {
        output::error(&provenance::refusal_message(&cur, &bless, &mismatches));
        return Err(DevError::ExitCode(REFUSED));
    }

    // Comparable. Report the diagnostic groups as context for whatever the
    // report says next - never as a verdict.
    for line in provenance::diagnostics(&cur, &bless) {
        output::run_msg(&format!("[contract] {line}"));
    }

    Ok(())
}

/// Read one archive's block and establish that it can carry a gate.
///
/// Two refusals, deliberately distinct. A *missing* block predates the schema
/// or failed to identify its input, and per the freshness rule is omitted
/// whole rather than written partially. A *present but incomplete* block is
/// the more dangerous case: it looks like evidence and is not, and if the
/// comparison ran on it, two blocks that each say nothing would agree about
/// everything.
fn read_gateable(path: &Path, which: &str) -> Result<Provenance, DevError> {
    let Some(prov) = Provenance::read(path)? else {
        output::error(&format!(
            "the {which} archive carries no elivagar provenance block, so it \
             cannot be gated: {}\n\
             It predates the block or failed to identify its input. Rebuild it \
             with `brokkr tilegen` (and re-bless, for a baseline) rather than \
             comparing on the assumption that the two are comparable.",
            path.display()
        ));
        return Err(DevError::ExitCode(REFUSED));
    };

    if let Err(problems) = prov.validate() {
        output::error(&format!(
            "the {which} archive's provenance block cannot support a gate: {}\n  {}",
            path.display(),
            problems.join("\n  ")
        ));
        return Err(DevError::ExitCode(REFUSED));
    }

    Ok(prov)
}
