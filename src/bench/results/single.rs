//! One script-bench run as a labelled block: identity, params, outcome and
//! walls, host and build, every iteration and its children, the result's
//! record and its `record.hotpath` report, and each child's captured
//! hotpath report.

use std::fmt::Write;

use super::super::store::{
    CaptureOutcome, Child, ChildEnding, ExecutableIdentity, IterationOutcome, MarkView,
    Measurement, RunDocument, RunOutcome, StderrCapture, StderrTail, StoredRun,
};
use super::{
    bench_name, hidden_by, hidden_detail, hotpath_report, ms, short_commit, stored_as, timestamp,
    verdict,
};

type Record = serde_json::Map<String, serde_json::Value>;

fn field(out: &mut String, label: &str, value: &str) {
    writeln!(out, "{label:<10}  {value}").expect("write to String is infallible");
}

/// A child's ending as every view words it.
pub(crate) fn ending(ending: &ChildEnding) -> String {
    match ending {
        ChildEnding::Exited { code } => format!("exit {code}"),
        ChildEnding::Signalled { signal } => format!("signal {signal}"),
        ChildEnding::StopMarker { marker } => format!("stopped at marker {marker}"),
        ChildEnding::Deadline { secs, nanos } => format!("deadline {secs}.{nanos:09} s"),
        ChildEnding::Interrupted => "interrupted".to_owned(),
        ChildEnding::WaitFailed { reason } => format!("wait failed: {reason}"),
    }
}

fn iteration_outcome(outcome: &IterationOutcome) -> String {
    match outcome {
        IterationOutcome::Completed { verdict: v } => {
            format!("completed, verdict {}", verdict(v.as_ref()))
        }
        IterationOutcome::Failed { reason } => format!("failed: {reason}"),
        IterationOutcome::Interrupted => "interrupted".to_owned(),
    }
}

fn params(out: &mut String, doc: &RunDocument) {
    for (i, (name, param)) in doc.params.iter().enumerate() {
        let values: Vec<String> = param
            .values
            .iter()
            .map(|v| match &v.digest {
                Some(digest) => format!("{}@{}", v.key, digest.chars().take(8).collect::<String>()),
                None => v.key.clone(),
            })
            .collect();
        let (open, close) = if param.many { ("[", "]") } else { ("", "") };
        let from = param
            .from
            .as_deref()
            .map_or_else(String::new, |from| format!(" (from {from})"));
        field(
            out,
            if i == 0 { "params" } else { "" },
            &format!("{name}={open}{}{close}{from}", values.join(",")),
        );
    }
}

/// Uuid, start, commit, bench, what hides it, params, outcome, the result's
/// wall and verdict.
fn identity(out: &mut String, run: &StoredRun, marks: &MarkView) {
    let doc = &run.doc;
    field(out, "uuid", &doc.uuid.chars().take(8).collect::<String>());
    field(out, "started", &timestamp(doc.started_unix_ms));
    let commit = match &doc.subject {
        Some(subject) => format!("{} ({subject})", short_commit(run)),
        None => short_commit(run),
    };
    field(out, "commit", &commit);
    field(
        out,
        "bench",
        &match stored_as(marks, run) {
            Some(stored) => format!(
                "{} (stored as {stored}; project {})",
                bench_name(marks, run),
                doc.project
            ),
            None => format!("{} (project {})", doc.bench, doc.project),
        },
    );
    for (i, (kind, origin)) in hidden_by(marks, run).into_iter().enumerate() {
        field(
            out,
            if i == 0 { "hidden" } else { "" },
            &hidden_detail(kind, origin),
        );
    }
    params(out, doc);
    let outcome = match &doc.outcome {
        RunOutcome::Completed => "completed".to_owned(),
        RunOutcome::Failed { reason } => format!("failed: {reason}"),
        RunOutcome::Interrupted => "interrupted".to_owned(),
    };
    field(out, "outcome", &outcome);
    match doc.measurement {
        Measurement::Timing => {}
        Measurement::Hotpath => field(out, "measured", "hotpath (--hotpath)"),
        Measurement::Alloc => field(out, "measured", "alloc (--alloc)"),
    }
    if let Some(best) = run.best_iteration {
        let wall = |w: Option<i64>| w.map_or_else(|| "--".to_owned(), ms);
        let walls: Vec<String> = doc
            .iterations
            .iter()
            .map(|i| wall(i.result_wall_us()))
            .collect();
        let basis = if doc.child_measured() {
            ", the measured call's wall"
        } else {
            ""
        };
        let detail = if walls.len() > 1 {
            format!(
                " (iteration {best}, best of {}: {})",
                walls.len(),
                walls.join(" / ")
            )
        } else {
            String::new()
        };
        field(
            out,
            "elapsed",
            &format!(
                "{}{detail}{basis}",
                wall(doc.iterations[best].result_wall_us())
            ),
        );
        if let IterationOutcome::Completed { verdict: v } = &doc.iterations[best].outcome {
            field(out, "verdict", &verdict(v.as_ref()));
        }
    }
    field(
        out,
        "iterations",
        &format!(
            "{} of {} requested",
            doc.iterations.len(),
            doc.iterations_requested
        ),
    );
}

/// Host, conditions, captured env, build, config and brokkr's invocation.
fn context(out: &mut String, run: &StoredRun) {
    let doc = &run.doc;
    let unknown = |v: Option<&String>| v.map_or("(unknown)", String::as_str).to_owned();
    field(out, "host", &doc.hostname);
    field(out, "checkout", &doc.checkout);
    let c = &doc.conditions;
    field(out, "kernel", &unknown(c.kernel.as_ref()));
    field(out, "governor", &unknown(c.cpu_governor.as_ref()));
    field(
        out,
        "memory",
        &c.avail_memory_mb
            .map_or_else(|| "(unknown)".to_owned(), |mb| format!("{mb} MB available")),
    );
    for (i, (name, value)) in c.captured_env.iter().enumerate() {
        field(
            out,
            if i == 0 { "env" } else { "" },
            &format!("{name}={value}"),
        );
    }
    let b = &doc.build;
    let mut build = format!(
        "{} {}, features [{}]{}",
        b.target.as_deref().unwrap_or("(default bin)"),
        b.profile,
        b.features.join(","),
        if b.default_features {
            ""
        } else {
            ", no default features"
        }
    );
    if let Some(resolved) = &b.resolved {
        write!(
            build,
            "; resolved {} with [{}]",
            resolved.bin,
            resolved.features.join(",")
        )
        .expect("write to String is infallible");
    }
    if let Some(artifact) = &b.artifact {
        write!(build, "; artifact {artifact}").expect("write to String is infallible");
    }
    field(out, "build", &build);
    field(out, "config", &format!("xxh3-128 {}", run.config_digest));
    field(
        out,
        "brokkr",
        &format!("{} {}", doc.brokkr_version, doc.argv.join(" ")),
    );
    // Where a child's evidence is read, as a typed block names its sidecar.
    if run.children_loaded && doc.iterations.iter().any(|i| !i.children.is_empty()) {
        field(
            out,
            "sidecar",
            &format!(
                "brokkr sidecar {} [--run ITER] [--child N] ({})",
                doc.uuid.chars().take(8).collect::<String>(),
                crate::sidecar_cmd::view_selector_hint()
            ),
        );
    }
}

/// One child: when it started and how long it ran, how it ended, what it
/// ran, and what its capture kept and lost.
fn child(out: &mut String, idx: usize, child: &Child) {
    let e = &child.evidence;
    let mut losses = Vec::new();
    if e.loss.rejected_lines > 0 {
        losses.push(format!("{} rejected lines", e.loss.rejected_lines));
    }
    for (lost, what) in [
        (e.loss.fifo_unread, "fifo unread"),
        (e.loss.records_capped, "records capped"),
        (e.loss.samples_capped, "samples capped"),
        (e.loss.output_truncated, "output truncated"),
        (e.calls_overflowed, "call census overflowed"),
    ] {
        if lost {
            losses.push(what.to_owned());
        }
    }
    if e.clock.outliers > 0 {
        losses.push(format!("{} clock outliers", e.clock.outliers));
    }
    let loss = if losses.is_empty() {
        String::new()
    } else {
        format!("; {}", losses.join(", "))
    };
    writeln!(
        out,
        "  child {idx}{}  at +{}  {}  {}  {} {}",
        if child.measured { " (measured)" } else { "" },
        ms(child.origin_offset_us),
        ms(child.wall_us),
        ending(&child.ending),
        child.program,
        child.argv.join(" ")
    )
    .expect("write to String is infallible");
    writeln!(
        out,
        "           {} samples, {} records{loss}",
        e.samples.len(),
        e.records.len()
    )
    .expect("write to String is infallible");
    if let Some(executable) = &child.executable {
        writeln!(
            out,
            "           executable {}: {}",
            executable.path,
            executable_identity(&executable.identity)
        )
        .expect("write to String is infallible");
    }
    if let Some(cache) = &child.cache {
        // Counts accumulated over the scan interval before launch, not one
        // snapshot, and nothing about the cache once the child ran.
        writeln!(
            out,
            "           page cache ({}), scanned +{} to +{} before launch:",
            cache.procedure,
            ms(cache.scan_from_us),
            ms(cache.scan_to_us)
        )
        .expect("write to String is infallible");
        for file in &cache.files {
            writeln!(
                out,
                "             {} {}: {} of {} bytes resident",
                file.mode.name(),
                file.paths.join(", "),
                file.resident,
                file.bytes
            )
            .expect("write to String is infallible");
        }
    }
    if let Some(capture) = &child.hotpath {
        let status = match &capture.outcome {
            CaptureOutcome::Captured { .. } => "captured".to_owned(),
            CaptureOutcome::Missing => "missing (no report was written)".to_owned(),
            CaptureOutcome::Invalid { reason } => format!("invalid: {reason}"),
            CaptureOutcome::Unreadable { reason } => format!("unreadable: {reason}"),
            CaptureOutcome::OverBudget => "over budget, not read".to_owned(),
        };
        writeln!(
            out,
            "           hotpath {} #{}: {status}",
            capture.stage, capture.occurrence
        )
        .expect("write to String is infallible");
    }
    if let Some(stderr) = &child.stderr {
        stderr_lines(out, stderr);
    }
}

/// The stderr tail lines a child's view shows at most, and their characters
/// together, escapes and omission markers included.
const STDERR_SHOWN_LINES: usize = 10;
const STDERR_SHOWN_CHARS: usize = 2000;

/// A child's stderr capture: its size and state, then the last few lines of
/// its kept tail, bounded and with control characters escaped.
fn stderr_lines(out: &mut String, stderr: &StderrCapture) {
    let size = stderr
        .bytes
        .map_or_else(|| "size unknown".to_owned(), |b| format!("{b} bytes"));
    let unreleased = if stderr.released {
        ""
    } else {
        ", snapshotted while its call was still held"
    };
    let (state, text) = match &stderr.tail {
        StderrTail::Kept { text, cut, .. } => (
            if *cut { "; its tail kept" } else { "" }.to_owned(),
            Some(text),
        ),
        StderrTail::OverBudget => ("; over the run's budget, not kept".to_owned(), None),
        StderrTail::Unreadable { reason } => (format!("; unreadable: {reason}"), None),
    };
    writeln!(out, "           stderr {size}{unreleased}{state}")
        .expect("write to String is infallible");
    let Some(text) = text else { return };
    let lines: Vec<&str> = text.lines().collect();
    let mut shown: Vec<String> = Vec::new();
    let mut budget = STDERR_SHOWN_CHARS;
    for line in lines.iter().rev().take(STDERR_SHOWN_LINES) {
        let escaped: Vec<char> = line.chars().flat_map(char::escape_debug).collect();
        if escaped.len() > budget && budget < 3 {
            break;
        }
        let line: String = if escaped.len() > budget {
            // Room for the marker, so the lines shown stay within the bound.
            let keep = budget.saturating_sub(3);
            escaped[..keep].iter().chain(['.'; 3].iter()).collect()
        } else {
            escaped.iter().collect()
        };
        budget = budget.saturating_sub(line.chars().count());
        shown.push(line);
        if budget == 0 {
            break;
        }
    }
    for line in shown.iter().rev() {
        writeln!(out, "           | {line}").expect("write to String is infallible");
    }
}

/// An external executable's identity as observed at launch.
pub(super) fn executable_identity(identity: &ExecutableIdentity) -> String {
    match identity {
        ExecutableIdentity::Digest { digest } => format!("digest {digest}"),
        ExecutableIdentity::Unread { reason } => format!("not read ({reason})"),
    }
}

/// Each captured report of `iterations`, rendered by the typed rows'
/// printer under its stage, occurrence and iteration; one brokkr cannot
/// render is named instead.
fn captures(out: &mut Vec<String>, run: &StoredRun, iterations: &[usize], top: usize) {
    for &idx in iterations {
        for child in &run.doc.iterations[idx].children {
            let Some(capture) = &child.hotpath else {
                continue;
            };
            let CaptureOutcome::Captured { report } = &capture.outcome else {
                continue;
            };
            let heading = format!(
                "hotpath {} #{} (iteration {idx})",
                capture.stage, capture.occurrence
            );
            let rendered = crate::db::hotpath_data_from_json(report)
                .and_then(|data| crate::hotpath_fmt::format_hotpath_report(&data, top));
            out.push(match rendered {
                Some(report) => format!("{heading}\n{report}"),
                None => format!("{heading}: holds nothing brokkr can render"),
            });
        }
    }
}

fn iterations(out: &mut String, run: &StoredRun) {
    for (idx, iteration) in run.doc.iterations.iter().enumerate() {
        let result = if run.best_iteration == Some(idx) {
            " (result)"
        } else {
            ""
        };
        let wall = match (iteration.wall_us, iteration.measured_child()) {
            (None, _) => "no wall".to_owned(),
            (Some(block), None) => ms(block),
            (Some(block), Some(child)) => format!("{} (block {})", ms(child.wall_us), ms(block)),
        };
        writeln!(
            out,
            "iteration {idx}{result}  {wall}  {}",
            iteration_outcome(&iteration.outcome)
        )
        .expect("write to String is infallible");
        if run.children_loaded {
            for (c, each) in iteration.children.iter().enumerate() {
                child(out, c, each);
            }
        }
    }
}

/// One record's entries, `hotpath` aside, as compact JSON; a readable
/// `record.hotpath` joins `reports`, an unreadable one is shown as it is.
fn record(out: &mut String, label: &str, record: &Record, top: usize, reports: &mut Vec<String>) {
    let entries: Vec<String> = record
        .iter()
        .filter(|(key, _)| key.as_str() != "hotpath")
        .map(|(key, value)| format!("{key} = {value}"))
        .collect();
    if entries.is_empty() && !record.contains_key("hotpath") {
        field(out, label, "(empty)");
    }
    for (i, entry) in entries.iter().enumerate() {
        field(out, if i == 0 { label } else { "" }, entry);
    }
    match hotpath_report(record, top) {
        Some(Ok(report)) => reports.push(report),
        Some(Err(raw)) => field(
            out,
            if entries.is_empty() { label } else { "" },
            &format!("hotpath = {raw} (not a hotpath report brokkr can read)"),
        ),
        None => {}
    }
}

/// Render one run in full, resolved against `marks`, the marks it was
/// selected under. Its children must be loaded to be shown.
pub(crate) fn format_single(run: &StoredRun, marks: &MarkView, top: usize) -> String {
    let doc = &run.doc;
    let mut out = String::new();
    identity(&mut out, run, marks);
    out.push('\n');
    context(&mut out, run);
    out.push('\n');
    iterations(&mut out, run);

    // The result's record, or for a run without one each record extracted.
    let records: Vec<(String, &Record)> = match run.best_iteration {
        Some(best) => doc.iterations[best]
            .record
            .iter()
            .map(|r| ("record".to_owned(), r))
            .collect(),
        None => doc
            .iterations
            .iter()
            .enumerate()
            .filter_map(|(i, it)| it.record.as_ref().map(|r| (format!("record {i}"), r)))
            .collect(),
    };
    if !records.is_empty() {
        out.push('\n');
    }
    let mut reports = Vec::new();
    for (label, each) in records {
        record(&mut out, &label, each, top, &mut reports);
    }
    if run.children_loaded {
        let shown: Vec<usize> = match run.best_iteration {
            Some(best) => vec![best],
            None => (0..doc.iterations.len()).collect(),
        };
        captures(&mut reports, run, &shown, top);
    }
    for report in reports {
        out.push('\n');
        out.push_str(&report);
        out.push('\n');
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}
