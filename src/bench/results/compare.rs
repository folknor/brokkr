//! Two commits' script-bench results side by side. Runs pair on their stored
//! pairing key alone (`reference/bench-store.md`): the latest run per key on
//! each side is compared, and what the key leaves out (the config, the
//! build, the host, the captured environment, the verdict, the record) is
//! named under the pair rather than splitting it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use super::super::store::{Build, CaptureOutcome, StoredRun};
use super::{config_diff, ms, params_summary, result_verdict, result_wall, verdict};

/// The order "latest" means everywhere: start time, then uuid.
fn order(run: &StoredRun) -> (i64, &str) {
    (run.doc.started_unix_ms, run.doc.uuid.as_str())
}

fn short(run: &StoredRun) -> String {
    run.doc.uuid.chars().take(8).collect()
}

/// What was asked of the build, which the pairing key leaves out.
fn requested(build: &Build) -> (&Option<String>, &str, &[String], bool) {
    (
        &build.target,
        build.profile.as_str(),
        build.features.as_slice(),
        build.default_features,
    )
}

/// Same captured environment and same requested build.
fn same_context(a: &StoredRun, b: &StoredRun) -> bool {
    a.doc.conditions.captured_env == b.doc.conditions.captured_env
        && requested(&a.doc.build) == requested(&b.doc.build)
}

fn change(a: Option<i64>, b: Option<i64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) if a != 0 => {
            #[allow(clippy::cast_precision_loss)]
            let pct = (b - a) as f64 / a as f64 * 100.0;
            if pct >= 0.0 {
                format!("+{pct:.1}%")
            } else {
                format!("{pct:.1}%")
            }
        }
        _ => "--".to_owned(),
    }
}

fn wall(run: Option<&StoredRun>) -> String {
    run.and_then(result_wall)
        .map_or_else(|| "--".to_owned(), ms)
}

fn opt(value: Option<&String>) -> &str {
    value.map_or("(unknown)", String::as_str)
}

fn build_line(a: &Build, b: &Build) -> Option<String> {
    let mut parts = Vec::new();
    if a.target != b.target {
        parts.push(format!(
            "target {} -> {}",
            a.target.as_deref().unwrap_or("(default bin)"),
            b.target.as_deref().unwrap_or("(default bin)")
        ));
    }
    if a.profile != b.profile {
        parts.push(format!("profile {} -> {}", a.profile, b.profile));
    }
    if a.features != b.features {
        parts.push(format!(
            "features [{}] -> [{}]",
            a.features.join(","),
            b.features.join(",")
        ));
    }
    if a.default_features != b.default_features {
        parts.push(format!(
            "default features {} -> {}",
            a.default_features, b.default_features
        ));
    }
    let resolved = |b: &Build| {
        b.resolved
            .as_ref()
            .map(|r| format!("{} [{}]", r.bin, r.features.join(",")))
    };
    if resolved(a) != resolved(b) {
        parts.push(format!(
            "resolved {} -> {}",
            resolved(a).unwrap_or_else(|| "(none)".to_owned()),
            resolved(b).unwrap_or_else(|| "(none)".to_owned())
        ));
    }
    (!parts.is_empty()).then(|| format!("  build differs: {}", parts.join(", ")))
}

fn host_line(a: &StoredRun, b: &StoredRun) -> Option<String> {
    let (ca, cb) = (&a.doc.conditions, &b.doc.conditions);
    let mut parts = Vec::new();
    if a.doc.hostname != b.doc.hostname {
        parts.push(format!("host {} vs {}", a.doc.hostname, b.doc.hostname));
    }
    match (ca.avail_memory_mb, cb.avail_memory_mb) {
        (Some(x), Some(y)) if x != y => {
            let delta = y - x;
            #[allow(clippy::cast_precision_loss)]
            let pct = if x == 0 {
                0.0
            } else {
                delta as f64 / x as f64 * 100.0
            };
            parts.push(format!(
                "memory {x} MB vs {y} MB ({delta:+} MB, {pct:+.1}%)"
            ));
        }
        (Some(x), None) => parts.push(format!("memory {x} MB vs (unknown)")),
        (None, Some(y)) => parts.push(format!("memory (unknown) vs {y} MB")),
        _ => {}
    }
    if ca.cpu_governor != cb.cpu_governor {
        parts.push(format!(
            "governor {} vs {}",
            opt(ca.cpu_governor.as_ref()),
            opt(cb.cpu_governor.as_ref())
        ));
    }
    if ca.kernel != cb.kernel {
        parts.push(format!(
            "kernel {} vs {}",
            opt(ca.kernel.as_ref()),
            opt(cb.kernel.as_ref())
        ));
    }
    (!parts.is_empty()).then(|| format!("  host: {}", parts.join(", ")))
}

fn env_line(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> Option<String> {
    let names: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let parts: Vec<String> = names
        .into_iter()
        .filter(|n| a.get(*n) != b.get(*n))
        .map(|n| {
            format!(
                "{n}={} vs {}",
                a.get(n).map_or("(unset)", String::as_str),
                b.get(n).map_or("(unset)", String::as_str)
            )
        })
        .collect();
    (!parts.is_empty()).then(|| format!("  env: {}", parts.join(", ")))
}

fn verdict_line(a: &StoredRun, b: &StoredRun) -> Option<String> {
    let (va, vb) = (result_verdict(a)?, result_verdict(b)?);
    (va != vb).then(|| {
        format!(
            "  verdict: {} -> {} (the sides measured different answers)",
            verdict(va),
            verdict(vb)
        )
    })
}

fn result_record(run: &StoredRun) -> Option<&serde_json::Map<String, serde_json::Value>> {
    run.doc.iterations.get(run.best_iteration?)?.record.as_ref()
}

/// Each captured report of a run's result iteration, by stage and
/// occurrence (`decode #1`).
fn result_captures(run: &StoredRun) -> BTreeMap<String, &serde_json::Value> {
    let Some(iteration) = run.best_iteration.and_then(|i| run.doc.iterations.get(i)) else {
        return BTreeMap::new();
    };
    iteration
        .children
        .iter()
        .filter_map(|child| {
            let capture = child.hotpath.as_ref()?;
            match &capture.outcome {
                CaptureOutcome::Captured { report } => {
                    Some((format!("{} #{}", capture.stage, capture.occurrence), report))
                }
                _ => None,
            }
        })
        .collect()
}

/// The function diff of every capture both result iterations hold under one
/// stage and occurrence, and a line naming those only one side holds. The
/// correspondence is by name and count: a call one side made conditionally
/// shifts the later occurrences of its stage.
fn capture_diffs(
    a: &StoredRun,
    b: &StoredRun,
    top: usize,
) -> (Vec<(String, String)>, Option<String>) {
    let (ca, cb) = (result_captures(a), result_captures(b));
    let mut diffs = Vec::new();
    let mut unmatched = Vec::new();
    let names: BTreeSet<&String> = ca.keys().chain(cb.keys()).collect();
    for name in names {
        match (ca.get(name), cb.get(name)) {
            (Some(x), Some(y)) => {
                let parsed =
                    crate::db::hotpath_data_from_json(x).zip(crate::db::hotpath_data_from_json(y));
                if let Some((hx, hy)) = parsed
                    && let Some(diff) = crate::hotpath_fmt::format_hotpath_diff(&hx, &hy, top)
                {
                    diffs.push((name.clone(), diff));
                }
            }
            (Some(_), None) => unmatched.push(format!("{name} on A only")),
            (None, Some(_)) => unmatched.push(format!("{name} on B only")),
            (None, None) => {}
        }
    }
    let line = (!unmatched.is_empty())
        .then(|| format!("  hotpath captures unpaired: {}", unmatched.join(", ")));
    (diffs, line)
}

/// Every external executable a run's children ran, by path, with each
/// identity observed for it across every iteration: one path observed twice
/// differently keeps both.
fn executables(run: &StoredRun) -> BTreeMap<&str, BTreeSet<String>> {
    let mut seen: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for iteration in &run.doc.iterations {
        for child in &iteration.children {
            if let Some(executable) = &child.executable {
                seen.entry(executable.path.as_str())
                    .or_default()
                    .insert(super::single::executable_identity(&executable.identity));
            }
        }
    }
    seen
}

/// The external executables whose observed identities differ between the
/// sides, or that only one side ran: provenance the pairing key leaves out,
/// as it leaves out the build.
fn executables_line(a: &StoredRun, b: &StoredRun) -> Option<String> {
    let (ea, eb) = (executables(a), executables(b));
    let paths: BTreeSet<&&str> = ea.keys().chain(eb.keys()).collect();
    let shown = |ids: Option<&BTreeSet<String>>| {
        ids.map_or_else(
            || "(not run)".to_owned(),
            |ids| ids.iter().cloned().collect::<Vec<_>>().join(" and "),
        )
    };
    // An identity that was not read is no evidence the file stayed the same,
    // so a path either side could not read is named even when both read
    // the same failure.
    let unread = |ids: Option<&BTreeSet<String>>| {
        ids.is_some_and(|ids| ids.iter().any(|id| !id.starts_with("digest ")))
    };
    let parts: Vec<String> = paths
        .into_iter()
        .filter(|p| ea.get(**p) != eb.get(**p) || unread(ea.get(**p)) || unread(eb.get(**p)))
        .map(|p| format!("{p} {} vs {}", shown(ea.get(*p)), shown(eb.get(*p))))
        .collect();
    (!parts.is_empty()).then(|| {
        format!(
            "  executables differ or were not read: {}",
            parts.join(", ")
        )
    })
}

/// The result records' entries that moved, `hotpath` aside: what turns "12%
/// faster" into "12% faster on 8% less work".
fn record_line(a: &StoredRun, b: &StoredRun) -> Option<String> {
    let empty = serde_json::Map::new();
    let (ra, rb) = (
        result_record(a).unwrap_or(&empty),
        result_record(b).unwrap_or(&empty),
    );
    let keys: BTreeSet<&String> = ra.keys().chain(rb.keys()).collect();
    let moved: Vec<String> = keys
        .into_iter()
        .filter(|k| k.as_str() != "hotpath" && ra.get(*k) != rb.get(*k))
        .map(|k| {
            let show = |v: Option<&serde_json::Value>| {
                v.map_or_else(|| "(absent)".to_owned(), ToString::to_string)
            };
            format!("{k} {} -> {}", show(ra.get(k)), show(rb.get(k)))
        })
        .collect();
    (!moved.is_empty()).then(|| format!("    record: {}", moved.join(", ")))
}

/// Runs under the key other than the shown one whose captured environment
/// or requested build differ from it: comparable arms the pair collapsed.
fn collapsed_line(
    shown: (&StoredRun, &StoredRun),
    a: &[&StoredRun],
    b: &[&StoredRun],
) -> Option<String> {
    let others = |runs: &[&StoredRun], shown: &StoredRun| -> (usize, Option<String>) {
        let differing: Vec<&&StoredRun> = runs[1..]
            .iter()
            .filter(|r| !same_context(r, shown))
            .collect();
        (differing.len(), differing.first().map(|r| short(r)))
    };
    let (na, newest_a) = others(a, shown.0);
    let (nb, newest_b) = others(b, shown.1);
    if na + nb == 0 {
        return None;
    }
    let side = |n: usize, newest: Option<String>| match newest {
        Some(uuid) => format!("{n} (newest {uuid})"),
        None => "0".to_owned(),
    };
    Some(format!(
        "  other runs under this key with a different env or build: A {}, B {}",
        side(na, newest_a),
        side(nb, newest_b)
    ))
}

/// When the shown pair differs in captured environment or requested build:
/// the pair under the key whose older run is newest among pairs equal in
/// both, with its own walls and remaining differences. A pointer, never
/// substituted for the shown pair.
fn pointer_line(
    shown: (&StoredRun, &StoredRun),
    a: &[&StoredRun],
    b: &[&StoredRun],
) -> Option<String> {
    if same_context(shown.0, shown.1) {
        return None;
    }
    fn older<'r>(p: (&'r StoredRun, &'r StoredRun)) -> (i64, &'r str) {
        order(p.0).min(order(p.1))
    }
    let mut best: Option<(&StoredRun, &StoredRun)> = None;
    for x in a {
        for y in b {
            if !same_context(x, y) {
                continue;
            }
            if best.is_none_or(|current| older((x, y)) > older(current)) {
                best = Some((x, y));
            }
        }
    }
    let (x, y) = best?;
    let mut line = format!(
        "  older pair with equal captured env and requested build: {} vs {} ({} -> {}, {})",
        short(x),
        short(y),
        wall(Some(x)),
        wall(Some(y)),
        change(result_wall(x), result_wall(y))
    );
    if let Some(build) = build_line(&x.doc.build, &y.doc.build) {
        write!(line, "; {}", build.trim_start()).expect("write to String is infallible");
    }
    if let Some(host) = host_line(x, y) {
        write!(line, "; {}", host.trim_start()).expect("write to String is infallible");
    }
    Some(line)
}

/// The class a run pairs in, read from its stored key: `timing` under
/// `bench-pair-v1`, else the class `bench-pair-v2` names.
fn pairing_class(run: &StoredRun) -> String {
    let value: Option<serde_json::Value> = serde_json::from_str(&run.pairing_key).ok();
    let parts = value.as_ref().and_then(serde_json::Value::as_array);
    match parts.and_then(|p| Some((p.first()?.as_str()?, p.get(1)?.as_str()))) {
        Some(("bench-pair-v1", _)) => "timing".to_owned(),
        Some(("bench-pair-v2", Some(class))) => class.to_owned(),
        _ => "(unknown)".to_owned(),
    }
}

/// Why a one-sided run may have no partner: every identity difference from
/// the other side's latest run of the same bench. That run is a candidate,
/// not proof of the cause.
fn unpaired_line(run: &StoredRun, other: &[StoredRun], other_side: &str) -> Option<String> {
    let latest = other
        .iter()
        .filter(|r| r.doc.bench == run.doc.bench)
        .max_by(|x, y| order(x).cmp(&order(y)))?;
    let (tag, other_tag) = (run.pairing_algorithm(), latest.pairing_algorithm());
    let mut parts = Vec::new();
    let (class, other_class) = (pairing_class(run), pairing_class(latest));
    if class != other_class {
        parts.push(format!("measured {class} vs {other_class}"));
    } else if tag != other_tag {
        parts.push(format!(
            "pairing algorithm {} vs {}",
            tag.as_deref().unwrap_or("(unreadable)"),
            other_tag.as_deref().unwrap_or("(unreadable)")
        ));
    }
    let names: BTreeSet<&String> = run
        .doc
        .params
        .keys()
        .chain(latest.doc.params.keys())
        .collect();
    let values = |p: &super::super::store::BoundParam| {
        p.values
            .iter()
            .map(|v| match &v.digest {
                Some(d) => format!("{}@{}", v.key, d.chars().take(8).collect::<String>()),
                None => v.key.clone(),
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    for name in names {
        match (run.doc.params.get(name), latest.doc.params.get(name)) {
            (Some(_), None) => parts.push(format!("{name} bound here, unbound there")),
            (None, Some(_)) => parts.push(format!("{name} unbound here, bound there")),
            (Some(p), Some(q)) => {
                if p.many != q.many {
                    let shape = |many: bool| if many { "collection" } else { "scalar" };
                    parts.push(format!("{name} {} vs {}", shape(p.many), shape(q.many)));
                }
                if p.values != q.values {
                    parts.push(format!("{name} {} vs {}", values(p), values(q)));
                }
            }
            (None, None) => {}
        }
    }
    if parts.is_empty() && tag == other_tag && class == other_class {
        parts.push("equal identity but a different stored key (an inconsistent key)".to_owned());
    }
    Some(format!(
        "  latest {} run on {other_side}, {}: {}",
        run.doc.bench,
        short(latest),
        parts.join("; ")
    ))
}

/// Render the comparison of two commits' runs, capped at `limit` pairs after
/// pairing. Both sides hold completed runs of a clean tree, newest first.
#[allow(clippy::too_many_lines)]
pub(crate) fn format_compare(
    commit_a: &str,
    runs_a: &[StoredRun],
    commit_b: &str,
    runs_b: &[StoredRun],
    top: usize,
    limit: usize,
) -> String {
    let mut a: BTreeMap<(&str, &str), Vec<&StoredRun>> = BTreeMap::new();
    let mut b: BTreeMap<(&str, &str), Vec<&StoredRun>> = BTreeMap::new();
    for (runs, map) in [(runs_a, &mut a), (runs_b, &mut b)] {
        for run in runs {
            map.entry((run.doc.bench.as_str(), run.pairing_key.as_str()))
                .or_default()
                .push(run);
        }
        for list in map.values_mut() {
            list.sort_by(|x, y| order(y).cmp(&order(x)));
        }
    }
    let keys: BTreeSet<(&str, &str)> = a.keys().chain(b.keys()).copied().collect();
    let total = keys.len();
    if total == 0 {
        return "(no results)".to_owned();
    }
    let none: Vec<&StoredRun> = Vec::new();
    let rows: Vec<(&Vec<&StoredRun>, &Vec<&StoredRun>)> = keys
        .iter()
        .take(limit)
        .map(|k| (a.get(k).unwrap_or(&none), b.get(k).unwrap_or(&none)))
        .collect();

    let mut table: Vec<[String; 5]> = vec![[
        "bench".to_owned(),
        "params".to_owned(),
        commit_a.to_owned(),
        commit_b.to_owned(),
        "change".to_owned(),
    ]];
    for (ra, rb) in &rows {
        let (x, y) = (ra.first().copied(), rb.first().copied());
        let any = x.or(y).expect("a key has a run on some side");
        table.push([
            any.doc.bench.clone(),
            params_summary(any),
            wall(x),
            wall(y),
            change(x.and_then(result_wall), y.and_then(result_wall)),
        ]);
    }
    let widths: Vec<usize> = (0..5)
        .map(|c| {
            table
                .iter()
                .map(|r| r[c].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let render = |row: &[String; 5]| {
        let pad = |c: usize, right: bool| {
            let fill = " ".repeat(widths[c].saturating_sub(row[c].chars().count()));
            if right {
                format!("{fill}{}", row[c])
            } else {
                format!("{}{fill}", row[c])
            }
        };
        format!(
            "{}  {}  {}  {}  {}",
            pad(0, false),
            pad(1, false),
            pad(2, true),
            pad(3, true),
            pad(4, true)
        )
    };

    let mut out = render(&table[0]);
    out.push('\n');
    let mut diffs = Vec::new();
    for (i, (ra, rb)) in rows.iter().enumerate() {
        out.push_str(render(&table[i + 1]).trim_end());
        out.push('\n');
        let mut lines = Vec::new();
        match (ra.first().copied(), rb.first().copied()) {
            (Some(x), Some(y)) => {
                lines.extend(config_diff::describe(
                    &x.config_digest,
                    &x.doc.config,
                    &y.config_digest,
                    &y.doc.config,
                ));
                lines.extend(build_line(&x.doc.build, &y.doc.build));
                lines.extend(executables_line(x, y));
                lines.extend(host_line(x, y));
                lines.extend(env_line(
                    &x.doc.conditions.captured_env,
                    &y.doc.conditions.captured_env,
                ));
                lines.extend(verdict_line(x, y));
                lines.extend(record_line(x, y));
                lines.extend(collapsed_line((x, y), ra, rb));
                lines.extend(pointer_line((x, y), ra, rb));
                let parse = |run: &StoredRun| {
                    result_record(run)
                        .and_then(|r| r.get("hotpath"))
                        .and_then(crate::db::hotpath_data_from_json)
                };
                let heading = format!(
                    "{} {} - {commit_a} vs {commit_b}",
                    x.doc.bench,
                    params_summary(x)
                );
                if let (Some(ha), Some(hb)) = (parse(x), parse(y))
                    && let Some(diff) = crate::hotpath_fmt::format_hotpath_diff(&ha, &hb, top)
                {
                    diffs.push(format!("{heading}\n{diff}"));
                }
                let (stage_diffs, unmatched) = capture_diffs(x, y, top);
                diffs.extend(
                    stage_diffs
                        .into_iter()
                        .map(|(stage, diff)| format!("{heading}, hotpath {stage}\n{diff}")),
                );
                lines.extend(unmatched);
            }
            (Some(x), None) => lines.extend(unpaired_line(x, runs_b, "B")),
            (None, Some(y)) => lines.extend(unpaired_line(y, runs_a, "A")),
            (None, None) => {}
        }
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
    }
    for diff in diffs {
        out.push('\n');
        out.push_str(diff.trim_end());
        out.push('\n');
    }
    let withheld = total.saturating_sub(limit);
    if withheld > 0 {
        writeln!(
            out,
            "\n({withheld} more pair{} not shown - raise --limit)",
            if withheld == 1 { "" } else { "s" }
        )
        .expect("write to String is infallible");
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}
