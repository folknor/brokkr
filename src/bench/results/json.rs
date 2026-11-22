//! `brokkr results UUID --json`: one script-bench run as a versioned JSON
//! document on stdout, for a project-local tool to gate on
//! (`docs/commands/results.md`, JSON). The run document is embedded whole;
//! beside it are what brokkr derives from it, so a reader never reimplements
//! result selection or a peak.

use serde_json::{Value, json};

use super::{BenchFilter, BenchReader, bench_name, hidden_by, result_verdict};
use crate::bench::store::{self, ChildEnding, StoredRun};
use crate::error::DevError;

/// The wrapper's schema name and version. A change a reader could misread
/// bumps the version; a field added beside the others does not.
pub(crate) const SCHEMA: &str = "brokkr-results-run";
pub(crate) const VERSION: u32 = 1;

/// The one run `prefix` names among the project's, hidden or not, as the
/// JSON document. Refused unless the prefix names exactly one run.
pub(crate) fn run_json(reader: &BenchReader, prefix: &str) -> Result<String, DevError> {
    let filter = BenchFilter {
        hidden: true,
        limit: 2,
        ..BenchFilter::default()
    };
    let lookup = reader.lookup(prefix, &filter)?;
    match (lookup.candidates, lookup.runs.as_slice()) {
        (1, [run]) => Ok(serde_json::to_string_pretty(&document(run, &lookup.marks))
            .map_err(|e| DevError::Database(format!("encoding run {}: {e}", run.doc.uuid)))?),
        (0, _) => Err(DevError::Usage(match lookup.elsewhere.first() {
            Some((uuid, project)) => format!(
                "script-bench run {uuid} belongs to project {project}; --json reads the current project's runs"
            ),
            None => format!("no script-bench run of this project starts with {prefix}"),
        })),
        (n, _) => Err(DevError::Usage(format!(
            "uuid prefix '{prefix}' names {n} script-bench runs; --json needs one"
        ))),
    }
}

fn document(run: &StoredRun, marks: &store::MarkView) -> Value {
    let doc = &run.doc;
    let hidden: Vec<Value> = hidden_by(marks, run)
        .into_iter()
        .map(|(by, origin)| {
            json!({
                "by": by,
                "mark": origin.uuid,
                "at_unix_ms": origin.at_unix_ms,
                "hostname": origin.hostname,
                "reason": origin.reason,
            })
        })
        .collect();
    let result = run.best_iteration.map(|idx| {
        json!({
            "iteration": idx,
            "wall_us": doc.iterations.get(idx).and_then(store::Iteration::result_wall_us),
            "wall_of": if doc.child_measured() { "measured_child" } else { "block" },
            "verdict": result_verdict(run).flatten(),
        })
    });
    let iterations: Vec<Value> = doc
        .iterations
        .iter()
        .enumerate()
        .map(|(idx, iteration)| {
            let children: Vec<Value> = iteration
                .children
                .iter()
                .enumerate()
                .map(|(cidx, child)| child_json(cidx, child))
                .collect();
            json!({
                "index": idx,
                "result": run.best_iteration == Some(idx),
                "outcome": iteration.outcome,
                "wall_us": iteration.wall_us,
                "result_wall_us": iteration.result_wall_us(),
                "record": iteration.record,
                "children": children,
            })
        })
        .collect();
    json!({
        "schema": SCHEMA,
        "version": VERSION,
        "uuid": doc.uuid,
        "project": doc.project,
        "bench": bench_name(marks, run),
        "stored_bench": doc.bench,
        "commit": doc.commit,
        "dirty": doc.dirty,
        "hostname": doc.hostname,
        "started_unix_ms": doc.started_unix_ms,
        "measurement": doc.measurement,
        "build": {
            "target": doc.build.target,
            "profile": doc.build.profile,
            "features": doc.build.features,
            "default_features": doc.build.default_features,
            "resolved": doc.build.resolved,
        },
        "outcome": doc.outcome,
        "iterations_requested": doc.iterations_requested,
        "hidden": hidden,
        "pairing_key": run.pairing_key,
        "result": result,
        "iterations": iterations,
        "document": doc,
    })
}

/// One child's summary: its ending, an exit code when it exited, and the
/// peaks over its sidecar samples, `null` with no sample.
fn child_json(index: usize, child: &store::Child) -> Value {
    let samples = &child.evidence.samples;
    let exit_code = match child.ending {
        ChildEnding::Exited { code } => Some(code),
        _ => None,
    };
    json!({
        "index": index,
        "program": child.program,
        "argv": child.argv,
        "measured": child.measured,
        "wall_us": child.wall_us,
        "ending": child.ending,
        "exit_code": exit_code,
        "samples": samples.len(),
        "peak_rss_kb": samples.iter().map(|s| s.rss_kb).max(),
        "peak_vm_hwm_kb": samples.iter().map(|s| s.vm_hwm_kb).max(),
    })
}
