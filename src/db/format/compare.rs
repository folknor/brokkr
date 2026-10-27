use super::super::types::KvClass;
use super::super::{HotpathData, KvPair, StoredRow, Wall};
use super::DatasetMatcher;
use super::table::{
    compute_rewrite_pct, find_output_bytes, format_blob_counts, format_input, format_wall,
};

/// Format side-by-side comparison of two commits, uncapped - the tests'
/// shorthand for [`format_compare_with_limit`].
#[cfg(test)]
pub fn format_compare(
    commit_a: &str,
    rows_a: &[StoredRow],
    commit_b: &str,
    rows_b: &[StoredRow],
    top: usize,
    matcher: &DatasetMatcher,
) -> String {
    format_compare_with_limit(commit_a, rows_a, commit_b, rows_b, top, usize::MAX, matcher)
}

/// Format a comparison while capping the number of matched pairs shown.
/// Pairing happens before the limit, so a row cap cannot remove one side of a
/// pair and change the comparison result.
pub fn format_compare_with_limit(
    commit_a: &str,
    rows_a: &[StoredRow],
    commit_b: &str,
    rows_b: &[StoredRow],
    top: usize,
    limit: usize,
    matcher: &DatasetMatcher,
) -> String {
    let matcher = &matcher.for_inputs(
        rows_a
            .iter()
            .chain(rows_b)
            .map(|r| r.input_file.as_str()),
    );
    let mut pairs = build_comparison_pairs(rows_a, rows_b, matcher);
    // The cap defaults to 20, so say when it bit: a comparison that silently
    // ends early reads as "these are all the commands that were measured".
    let withheld = pairs.len().saturating_sub(limit);
    pairs.truncate(limit);
    if pairs.is_empty() {
        return String::from("(no results)");
    }

    let widths = compute_compare_widths(commit_a, commit_b, &pairs);
    let mut out = String::new();

    append_compare_header(&mut out, commit_a, commit_b, &widths);
    out.push('\n');

    for pair in &pairs {
        append_compare_row(&mut out, pair, &widths);
        out.push('\n');
        append_pair_annotations(&mut out, pair);
    }

    // Append hotpath diff tables for pairs that have hotpath data on both sides.
    for pair in &pairs {
        if let (Some(ha), Some(hb)) = (&pair.a_hotpath, &pair.b_hotpath)
            && let Some(diff) = crate::hotpath_fmt::format_hotpath_diff(ha, hb, top)
        {
            let cmd = &pair.key.command;
            let var = &pair.key.mode;
            let label = if var.is_empty() {
                cmd.to_owned()
            } else {
                format!("{cmd} {var}")
            };
            let heading = if pair.input_display.is_empty() {
                format!("\n{label} - {commit_a} vs {commit_b}")
            } else {
                format!(
                    "\n{label} - {} - {commit_a} vs {commit_b}",
                    pair.input_display
                )
            };
            out.push_str(&heading);
            out.push('\n');
            out.push_str(&diff);
        }
    }

    if withheld > 0 {
        out.push_str(&format!(
            "\n({withheld} more pair{} not shown - raise --limit)\n",
            if withheld == 1 { "" } else { "s" }
        ));
    }

    if out.ends_with('\n') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Compare formatting internals
// ---------------------------------------------------------------------------

struct CompareWidths {
    command: usize,
    mode: usize,
    input: usize,
    col_a: usize,
    col_b: usize,
    change: usize,
    has_output: bool,
    output_a: usize,
    output_b: usize,
    output_change: usize,
    has_rss: bool,
    rss_a: usize,
    rss_b: usize,
    rss_change: usize,
    has_rewrite: bool,
    rewrite_a: usize,
    rewrite_b: usize,
    has_blobs: bool,
    blobs_a: usize,
    blobs_b: usize,
}

struct ComparisonPair {
    key: PairKey,
    a_wall: Option<Wall>,
    b_wall: Option<Wall>,
    a_hotpath: Option<HotpathData>,
    b_hotpath: Option<HotpathData>,
    a_output_bytes: Option<i64>,
    b_output_bytes: Option<i64>,
    a_rss_mb: Option<f64>,
    b_rss_mb: Option<f64>,
    a_rewrite_pct: Option<f64>,
    b_rewrite_pct: Option<f64>,
    a_blobs: Option<String>,
    b_blobs: Option<String>,
    /// Pre-formatted input string for display.
    input_display: String,
    /// Captured env on each side. Same-env pairs render without an env
    /// line; differing pairs get a per-pair annotation.
    a_env: std::collections::BTreeMap<String, String>,
    b_env: std::collections::BTreeMap<String, String>,
    /// Host conditions on each side, annotated like `a_env`/`b_env` when
    /// they differ.
    a_host: HostEnv,
    b_host: HostEnv,
    /// Work-size counters on each side. See [`format_counter_diff`].
    a_counters: std::collections::BTreeMap<String, String>,
    b_counters: std::collections::BTreeMap<String, String>,
    /// `meta.exit_code` on each side, for a command whose exit status is its
    /// answer. See [`format_answer_diff`].
    a_exit: Option<String>,
    b_exit: Option<String>,
    /// Build provenance on each side (`provenance.*`, bare names); empty
    /// for a row that recorded none. See [`format_provenance_diff`].
    a_provenance: std::collections::BTreeMap<String, String>,
    b_provenance: std::collections::BTreeMap<String, String>,
    /// When the two compared rows' recorded provenance differs: the most
    /// recent older pair under the same key whose rows both recorded equal
    /// provenance, already rendered.
    like_for_like: Option<String>,
}

/// The full identity of a comparison row. Keeping the components separate
/// makes tabs and other delimiter characters in stored values unambiguous.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PairKey {
    command: String,
    mode: String,
    input_file: String,
    arm_key: String,
    env_fingerprint: String,
    build: PairBuild,
}

/// How a row's binary was built, as a pairing eligibility key. Two rows pair
/// only on equal known builds: a config-selected profile changes the timing
/// without changing the argv, so the arm key cannot carry it. Label equality
/// is necessary, not sufficient - two commits can define the same custom
/// profile differently.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum PairBuild {
    Known(crate::build::BuildKind),
    /// No stored profile, or one brokkr cannot read: nothing is known about
    /// how the binary was built. Unique per row, so it pairs with nothing -
    /// not even another row carrying the same unknown text. `raw` is `None`
    /// for a row with no profile at all.
    Unknown {
        side: Side,
        row_id: i64,
        raw: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Side {
    A,
    B,
}

impl PairBuild {
    fn of(row: &StoredRow, side: Side) -> Self {
        match &row.cargo_profile {
            Some(crate::db::types::StoredProfile::Known(kind)) => Self::Known(kind.clone()),
            other => Self::Unknown {
                side,
                row_id: row.id,
                raw: match other {
                    Some(crate::db::types::StoredProfile::Unknown(raw)) => Some(raw.clone()),
                    _ => None,
                },
            },
        }
    }

    /// The line a pair gets when its build is not the default release, so
    /// rows a profile split apart say why they did not pair.
    fn annotation(&self) -> Option<String> {
        match self {
            Self::Known(kind) if *kind == crate::build::BuildKind::default() => None,
            Self::Known(kind) => Some(format!("  build: {}", kind.as_label())),
            Self::Unknown { raw: Some(raw), .. } => {
                Some(format!("  build: unknown ({raw}), pairs with nothing"))
            }
            Self::Unknown { raw: None, .. } => {
                Some("  build: not recorded, pairs with nothing".to_owned())
            }
        }
    }
}

/// The host-condition fields of a row, pulled out for pairwise diffing.
///
/// These are recorded per run but were previously only visible in the
/// single-result view, so `--compare A B` could report a delta while silently
/// omitting that the two runs saw different amounts of free memory - which is
/// frequently the actual cause. Available memory in particular has been
/// observed to track wall time better than the commit under test does.
#[derive(Clone, Default, PartialEq, Eq)]
struct HostEnv {
    memory_mb: Option<i64>,
    governor: String,
    kernel: String,
}

/// The per-side fields a `ComparisonPair` is assembled from, pre-computed once
/// per row so the pairing loop below stays a pairing loop.
struct RowData {
    elapsed: Wall,
    hotpath: Option<HotpathData>,
    output_bytes: Option<i64>,
    peak_rss_mb: Option<f64>,
    rewrite_pct: Option<f64>,
    blobs: Option<String>,
    input_display: String,
    captured_env: std::collections::BTreeMap<String, String>,
    host: HostEnv,
    /// Every non-provenance counter on this row, stringified for comparison.
    counters: std::collections::BTreeMap<String, String>,
    exit: Option<String>,
    provenance: std::collections::BTreeMap<String, String>,
}

/// A row's recorded answer exit code (`meta.exit_code`), when it has one.
fn recorded_exit(kv: &[KvPair]) -> Option<String> {
    kv.iter()
        .find(|p| {
            p.class() == KvClass::Metadata && p.key.name() == crate::measure::capture::EXIT_CODE_KEY
        })
        .map(|p| p.value.to_string())
}

/// A row's build provenance, by bare name. Empty when it recorded none.
fn collect_provenance(kv: &[KvPair]) -> std::collections::BTreeMap<String, String> {
    kv.iter()
        .filter(|p| p.class() == KvClass::Provenance)
        .map(|p| (KvClass::Provenance.suffix(&p.key).to_owned(), p.value.to_string()))
        .collect()
}

/// The older pair a provenance-differing comparison can be read against:
/// the most recent (A, B) rows under the same key, other than the two
/// compared, that both recorded provenance and recorded it equal. Absent
/// provenance is never equal to anything - it is unknown, not a match.
/// Named by short ids with the pair's host differences, and never
/// substituted for the comparison shown: the latest rows are what compare.
/// `None` unless the compared rows (the first of each side) both recorded
/// provenance and it differs.
fn like_for_like(rows_a: &[&StoredRow], rows_b: &[&StoredRow]) -> Option<String> {
    let (latest_a, latest_b) = (
        collect_provenance(&rows_a.first()?.kv),
        collect_provenance(&rows_b.first()?.kv),
    );
    if latest_a.is_empty() || latest_b.is_empty() || latest_a == latest_b {
        return None;
    }
    // Recency by stored row id, which one results.db hands out in insertion
    // order across both sides: the pair whose older member is newest wins,
    // then the one whose newer member is. List positions say nothing across
    // two independently filled histories.
    let mut best: Option<((i64, i64), &StoredRow, &StoredRow)> = None;
    for (i, a) in rows_a.iter().enumerate() {
        let pa = collect_provenance(&a.kv);
        if pa.is_empty() {
            continue;
        }
        for (j, b) in rows_b.iter().enumerate() {
            if (i == 0 && j == 0) || collect_provenance(&b.kv) != pa {
                continue;
            }
            let rank = (a.id.min(b.id), a.id.max(b.id));
            if best.is_none_or(|(r, _, _)| rank > r) {
                best = Some((rank, a, b));
            }
        }
    }
    let (_, a, b) = best?;
    let host = |row: &StoredRow| HostEnv {
        memory_mb: row.avail_memory_mb,
        governor: row.cpu_governor.clone(),
        kernel: row.kernel.clone(),
    };
    let mut line = format!(
        "  older pair with equal recorded build provenance: {} vs {}",
        super::super::types::short_uuid(&a.uuid),
        super::super::types::short_uuid(&b.uuid)
    );
    if let Some(diff) = format_host_diff(&host(a), &host(b)) {
        line.push_str(&format!(" ({})", diff.trim_start()));
    }
    Some(line)
}

/// Collect a row's runtime counters, keyed by name.
///
/// Only unprefixed runtime counters describe the work. Metadata, captured env,
/// predecessor state, and thread summaries are separate comparison axes or
/// display data and must not be reported as moved work-size counters.
fn collect_counters(kv: &[KvPair]) -> std::collections::BTreeMap<String, String> {
    kv.iter()
        .filter(|p| p.class() == KvClass::Counter)
        .map(|p| (p.key.as_str().to_owned(), p.value.to_string()))
        .collect()
}

fn make_row_data(row: &StoredRow, matcher: &DatasetMatcher) -> RowData {
    RowData {
        elapsed: row.elapsed,
        hotpath: row.hotpath.clone(),
        output_bytes: find_output_bytes(&row.kv),
        peak_rss_mb: row.peak_rss_mb,
        rewrite_pct: compute_rewrite_pct(&row.kv),
        blobs: format_blob_counts(&row.kv),
        input_display: format_input(&row.input_file, row.input_mb, matcher),
        captured_env: row.captured_env.clone(),
        counters: collect_counters(&row.kv),
        exit: recorded_exit(&row.kv),
        provenance: collect_provenance(&row.kv),
        host: HostEnv {
            memory_mb: row.avail_memory_mb,
            governor: row.cpu_governor.clone(),
            kernel: row.kernel.clone(),
        },
    }
}

fn build_comparison_pairs(
    rows_a: &[StoredRow],
    rows_b: &[StoredRow],
    matcher: &DatasetMatcher,
) -> Vec<ComparisonPair> {
    use std::collections::HashMap;

    let row_key = |row: &StoredRow, side: Side| PairKey {
        build: PairBuild::of(row, side),
        ..pair_key(
            &row.command,
            &row.mode,
            &row.input_file,
            row.arm_key.as_deref().unwrap_or(row.brokkr_args.as_str()),
            &row.env_fingerprint(),
        )
    };

    let mut keys: Vec<PairKey> = Vec::new();
    let mut a_map: HashMap<PairKey, RowData> = HashMap::new();
    let mut b_map: HashMap<PairKey, RowData> = HashMap::new();
    // Every row per key, latest first, for the like-for-like lookup; the
    // first of each is the one compared.
    let mut a_rows: HashMap<PairKey, Vec<&StoredRow>> = HashMap::new();
    let mut b_rows: HashMap<PairKey, Vec<&StoredRow>> = HashMap::new();

    for row in rows_a {
        let key = row_key(row, Side::A);
        a_rows.entry(key.clone()).or_default().push(row);
        if let std::collections::hash_map::Entry::Vacant(e) = a_map.entry(key.clone()) {
            keys.push(key);
            e.insert(make_row_data(row, matcher));
        }
    }
    for row in rows_b {
        let key = row_key(row, Side::B);
        b_rows.entry(key.clone()).or_default().push(row);
        if let std::collections::hash_map::Entry::Vacant(e) = b_map.entry(key.clone()) {
            if !a_map.contains_key(&key) {
                keys.push(key.clone());
            }
            e.insert(make_row_data(row, matcher));
        }
    }

    keys.into_iter()
        .map(|k| {
            let a = a_map.remove(&k);
            let b = b_map.remove(&k);
            let a_provenance = a.as_ref().map(|r| r.provenance.clone()).unwrap_or_default();
            let b_provenance = b.as_ref().map(|r| r.provenance.clone()).unwrap_or_default();
            let like_for_like = like_for_like(
                a_rows.get(&k).map_or(&[][..], Vec::as_slice),
                b_rows.get(&k).map_or(&[][..], Vec::as_slice),
            );
            let input_display = a
                .as_ref()
                .or(b.as_ref())
                .map(|r| r.input_display.clone())
                .unwrap_or_default();
            let a_output_bytes = a.as_ref().and_then(|r| r.output_bytes);
            let b_output_bytes = b.as_ref().and_then(|r| r.output_bytes);
            let a_rss_mb = a.as_ref().and_then(|r| r.peak_rss_mb);
            let b_rss_mb = b.as_ref().and_then(|r| r.peak_rss_mb);
            let a_rewrite_pct = a.as_ref().and_then(|r| r.rewrite_pct);
            let b_rewrite_pct = b.as_ref().and_then(|r| r.rewrite_pct);
            let a_blobs = a.as_ref().and_then(|r| r.blobs.clone());
            let b_blobs = b.as_ref().and_then(|r| r.blobs.clone());
            let a_env = a
                .as_ref()
                .map(|r| r.captured_env.clone())
                .unwrap_or_default();
            let b_env = b
                .as_ref()
                .map(|r| r.captured_env.clone())
                .unwrap_or_default();
            let a_host = a.as_ref().map(|r| r.host.clone()).unwrap_or_default();
            let b_host = b.as_ref().map(|r| r.host.clone()).unwrap_or_default();
            let a_counters = a.as_ref().map(|r| r.counters.clone()).unwrap_or_default();
            let b_counters = b.as_ref().map(|r| r.counters.clone()).unwrap_or_default();
            let a_exit = a.as_ref().and_then(|r| r.exit.clone());
            let b_exit = b.as_ref().and_then(|r| r.exit.clone());
            ComparisonPair {
                key: k,
                a_wall: a.as_ref().map(|r| r.elapsed),
                b_wall: b.as_ref().map(|r| r.elapsed),
                a_hotpath: a.and_then(|r| r.hotpath),
                b_hotpath: b.and_then(|r| r.hotpath),
                a_output_bytes,
                b_output_bytes,
                a_rss_mb,
                b_rss_mb,
                a_rewrite_pct,
                b_rewrite_pct,
                a_blobs,
                b_blobs,
                input_display,
                a_env,
                b_env,
                a_host,
                b_host,
                a_counters,
                b_counters,
                a_exit,
                b_exit,
                a_provenance,
                b_provenance,
                like_for_like,
            }
        })
        .collect()
}

/// Append the per-pair `env:` / `host:` annotation lines under a compare row.
///
/// Both are skipped when the pair is one-sided - the row already shows `--` for
/// the missing side's elapsed, so "env: X=1 vs (unset)" would just duplicate
/// that signal, and with no B elapsed there is nothing for the host conditions
/// to explain.
fn append_pair_annotations(out: &mut String, pair: &ComparisonPair) {
    if let Some(annotation) = pair.key.build.annotation() {
        out.push_str(&annotation);
        out.push('\n');
    }
    if pair.a_wall.is_none() || pair.b_wall.is_none() {
        return;
    }
    if let Some(annotation) = format_env_diff(&pair.a_env, &pair.b_env) {
        out.push_str(&annotation);
        out.push('\n');
    }
    if let Some(annotation) = format_host_diff(&pair.a_host, &pair.b_host) {
        out.push_str(&annotation);
        out.push('\n');
    }
    if let Some(annotation) = format_answer_diff(pair.a_exit.as_deref(), pair.b_exit.as_deref()) {
        out.push_str(&annotation);
        out.push('\n');
    }
    if let Some(annotation) = format_counter_diff(&pair.a_counters, &pair.b_counters) {
        out.push_str(&annotation);
        out.push('\n');
    }
    if let Some(annotation) = format_provenance_diff(&pair.a_provenance, &pair.b_provenance) {
        out.push_str(&annotation);
        out.push('\n');
    }
    if let Some(line) = &pair.like_for_like {
        out.push_str(line);
        out.push('\n');
    }
}

/// Format the build-provenance line: every field that differs between two
/// rows that both recorded provenance, or which side recorded none.
///
/// Reported, never a reason not to pair. The inputs it covers are exactly
/// what a legitimate A/B can vary - a toolchain-bump commit, a commit adding
/// `-Ctarget-cpu=native` to the repository's config - so the comparison stays
/// possible and says what differed. Rows from before provenance was recorded
/// on both sides print nothing: there is nothing to tell apart.
fn format_provenance_diff(
    a: &std::collections::BTreeMap<String, String>,
    b: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return None,
        (true, false) => return Some("  build provenance: not recorded on A".to_owned()),
        (false, true) => return Some("  build provenance: not recorded on B".to_owned()),
        (false, false) => {}
    }
    let mut names: std::collections::BTreeSet<&str> = a.keys().map(String::as_str).collect();
    names.extend(b.keys().map(String::as_str));
    let differing: Vec<String> = names
        .into_iter()
        .filter_map(|name| {
            let (av, bv) = (a.get(name), b.get(name));
            (av != bv).then(|| {
                format!(
                    "{name} {} -> {}",
                    av.map_or("(absent)", String::as_str),
                    bv.map_or("(absent)", String::as_str)
                )
            })
        })
        .collect();
    if differing.is_empty() {
        return None;
    }
    Some(format!("  build provenance differs: {}", differing.join(", ")))
}

/// Format the counter line: every work-size counter that moved between the two
/// sides.
///
/// Reported, never fatal. A wall on its own cannot distinguish "the code got
/// faster" from "the code did less", and the counters beside it are what turn
/// "12% faster" into "12% faster on 8% fewer cells".
///
/// Deliberately not a gate. A predecessor let a workload declare which counters
/// were identity-bearing, so that a move in one of them made the pair an error
/// rather than a reading. The declaration only controlled fatality, and a gate
/// that fires on the first legitimate win - doing less work is what most
/// optimization past the free-lane stage actually is - earns a bypass flag and
/// then gets passed out of habit. Where a moved count really does invalidate a
/// series, the project owns that rule: for a seeded tape it is a protocol
/// version bump, which is unconditional and cannot be waived per comparison.
///
/// A counter on one side only is reported too: that is what instrumentation
/// appearing or vanishing mid-series looks like, and it makes the pair no more
/// comparable than a changed value does.
fn format_counter_diff(
    a: &std::collections::BTreeMap<String, String>,
    b: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let mut names: std::collections::BTreeSet<&str> = a.keys().map(String::as_str).collect();
    names.extend(b.keys().map(String::as_str));

    let mut moved: Vec<String> = Vec::new();
    for name in names {
        match (a.get(name), b.get(name)) {
            (Some(av), Some(bv)) if av == bv => {}
            (Some(av), Some(bv)) => moved.push(format!("{name} {av} -> {bv}")),
            (Some(av), None) => moved.push(format!("{name} {av} -> (absent)")),
            (None, Some(bv)) => moved.push(format!("{name} (absent) -> {bv}")),
            (None, None) => {}
        }
    }
    if moved.is_empty() {
        return None;
    }
    Some(format!("    counters: {}", moved.join(", ")))
}

/// Flag a pair whose two sides measured different answers: one `check` found
/// violations and the other did not, one `diff` found differences and the
/// other none. Their walls time different work. Only a value on both sides is
/// compared - a row stored before exit codes were recorded has none, and a
/// one-sided key says nothing about the answer.
fn format_answer_diff(a: Option<&str>, b: Option<&str>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) if a != b => Some(format!(
            "    answer: exit {a} -> {b} (the sides measured different answers)"
        )),
        _ => None,
    }
}

/// Format a per-pair env annotation when A and B captured different
/// env sets. Returns `None` when the sets are identical (the common
/// case - captured_env is empty on >95% of historical rows). The
/// emitted line sits under the compare row, indented two spaces.
fn format_env_diff(
    a: &std::collections::BTreeMap<String, String>,
    b: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    if a == b {
        return None;
    }
    let mut keys: std::collections::BTreeSet<&str> = a.keys().map(String::as_str).collect();
    keys.extend(b.keys().map(String::as_str));
    let mut parts: Vec<String> = Vec::new();
    for key in keys {
        let av = a.get(key);
        let bv = b.get(key);
        if av == bv {
            continue;
        }
        let a_str = av.map_or("(unset)", String::as_str);
        let b_str = bv.map_or("(unset)", String::as_str);
        parts.push(format!("{key}={a_str} vs {b_str}"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("  env: {}", parts.join(", ")))
}

/// Format a per-pair host-condition annotation when A and B ran under
/// different conditions. Returns `None` when they match.
///
/// Only differing fields are listed, so the common case (same box, same
/// governor, same kernel) prints nothing but a memory line. Memory is the
/// field that usually differs and the one most worth seeing: when two runs of
/// the same command disagree, "how much RAM was free" is the first question,
/// and the answer has repeatedly turned out to be the whole story.
///
/// Governor and kernel are near-constant in practice, so a difference in
/// either is worth shouting about - it invalidates the comparison outright.
fn format_host_diff(a: &HostEnv, b: &HostEnv) -> Option<String> {
    if a == b {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();

    match (a.memory_mb, b.memory_mb) {
        (Some(am), Some(bm)) if am != bm => {
            let delta = bm - am;
            #[allow(clippy::cast_precision_loss)]
            let pct = if am == 0 {
                0.0
            } else {
                (delta as f64 / am as f64) * 100.0
            };
            parts.push(format!(
                "memory {am} MB vs {bm} MB ({delta:+} MB, {pct:+.1}%)"
            ));
        }
        // One side predates the column (or wasn't recorded). There's no delta
        // to compute, but the asymmetry is still worth showing - staying silent
        // would claim the hosts matched when we simply don't know.
        (Some(am), None) => parts.push(format!("memory {am} MB vs (unknown)")),
        (None, Some(bm)) => parts.push(format!("memory (unknown) vs {bm} MB")),
        _ => {}
    }
    if a.governor != b.governor {
        parts.push(format!(
            "governor {} vs {}",
            display_or_unknown(&a.governor),
            display_or_unknown(&b.governor)
        ));
    }
    if a.kernel != b.kernel {
        parts.push(format!(
            "kernel {} vs {}",
            display_or_unknown(&a.kernel),
            display_or_unknown(&b.kernel)
        ));
    }

    if parts.is_empty() {
        return None;
    }
    Some(format!("  host: {}", parts.join(", ")))
}

/// Render an empty host field as `(unknown)` - older rows predate the
/// column, and a bare `vs` with a blank side reads as a formatting bug.
fn display_or_unknown(s: &str) -> &str {
    if s.is_empty() { "(unknown)" } else { s }
}

/// Build the comparison identity from the parsed-CLI arm key. Historical rows
/// without one use their exact stored invocation string; no argv grammar is
/// inferred here.
fn pair_key(command: &str, mode: &str, input_file: &str, arm_key: &str, env_fp: &str) -> PairKey {
    PairKey {
        command: command.to_owned(),
        mode: mode.to_owned(),
        input_file: input_file.to_owned(),
        arm_key: arm_key.to_owned(),
        env_fingerprint: env_fp.to_owned(),
        build: PairBuild::Known(crate::build::BuildKind::default()),
    }
}

fn compute_compare_widths(
    commit_a: &str,
    commit_b: &str,
    pairs: &[ComparisonPair],
) -> CompareWidths {
    let has_output = pairs
        .iter()
        .any(|p| p.a_output_bytes.is_some() || p.b_output_bytes.is_some());
    let has_rss = pairs
        .iter()
        .any(|p| p.a_rss_mb.is_some() || p.b_rss_mb.is_some());
    let has_rewrite = pairs
        .iter()
        .any(|p| p.a_rewrite_pct.is_some() || p.b_rewrite_pct.is_some());
    let has_blobs = pairs
        .iter()
        .any(|p| p.a_blobs.is_some() || p.b_blobs.is_some());
    let mut w = CompareWidths {
        command: 7,
        mode: 7,
        input: "dataset".len(),
        col_a: commit_a.len().max(2),
        col_b: commit_b.len().max(2),
        change: 6,
        has_output,
        output_a: if has_output { "output_a".len() } else { 0 },
        output_b: if has_output { "output_b".len() } else { 0 },
        output_change: if has_output { "out_chg".len() } else { 0 },
        has_rss,
        rss_a: if has_rss { "rss_a".len() } else { 0 },
        rss_b: if has_rss { "rss_b".len() } else { 0 },
        rss_change: if has_rss { "rss_chg".len() } else { 0 },
        has_rewrite,
        rewrite_a: if has_rewrite { "rewrite_a".len() } else { 0 },
        rewrite_b: if has_rewrite { "rewrite_b".len() } else { 0 },
        has_blobs,
        blobs_a: if has_blobs { "blobs_a".len() } else { 0 },
        blobs_b: if has_blobs { "blobs_b".len() } else { 0 },
    };
    for pair in pairs {
        let cmd = &pair.key.command;
        let var = &pair.key.mode;
        w.command = w.command.max(cmd.len());
        w.mode = w.mode.max(var.len());
        w.input = w.input.max(pair.input_display.len());
        w.col_a = w.col_a.max(format_ms_or_dash(pair.a_wall).len());
        w.col_b = w.col_b.max(format_ms_or_dash(pair.b_wall).len());
        w.change = w.change.max(format_change_pair(pair).len());
        if has_output {
            w.output_a = w
                .output_a
                .max(format_bytes_or_dash(pair.a_output_bytes).len());
            w.output_b = w
                .output_b
                .max(format_bytes_or_dash(pair.b_output_bytes).len());
            w.output_change = w
                .output_change
                .max(format_change_bytes(pair.a_output_bytes, pair.b_output_bytes).len());
        }
        if has_rss {
            w.rss_a = w.rss_a.max(format_rss_or_dash(pair.a_rss_mb).len());
            w.rss_b = w.rss_b.max(format_rss_or_dash(pair.b_rss_mb).len());
            w.rss_change = w
                .rss_change
                .max(format_change_rss(pair.a_rss_mb, pair.b_rss_mb).len());
        }
        if has_rewrite {
            w.rewrite_a = w
                .rewrite_a
                .max(format_pct_or_dash(pair.a_rewrite_pct).len());
            w.rewrite_b = w
                .rewrite_b
                .max(format_pct_or_dash(pair.b_rewrite_pct).len());
        }
        if has_blobs {
            w.blobs_a = w.blobs_a.max(format_opt_str_or_dash(&pair.a_blobs).len());
            w.blobs_b = w.blobs_b.max(format_opt_str_or_dash(&pair.b_blobs).len());
        }
    }
    w
}

fn append_compare_header(out: &mut String, commit_a: &str, commit_b: &str, w: &CompareWidths) {
    use std::fmt::Write;
    write!(
        out,
        "{:<cmd_w$}  {:<var_w$}  {:<in_w$}  {:>a_w$}  {:>b_w$}  {:>ch_w$}",
        "command",
        "mode",
        "dataset",
        commit_a,
        commit_b,
        "change",
        cmd_w = w.command,
        var_w = w.mode,
        in_w = w.input,
        a_w = w.col_a,
        b_w = w.col_b,
        ch_w = w.change,
    )
    .expect("write to String is infallible");
    if w.has_output {
        write!(
            out,
            "  {:>oa_w$}  {:>ob_w$}  {:>oc_w$}",
            "output_a",
            "output_b",
            "out_chg",
            oa_w = w.output_a,
            ob_w = w.output_b,
            oc_w = w.output_change,
        )
        .expect("write to String is infallible");
    }
    if w.has_rss {
        write!(
            out,
            "  {:>ra_w$}  {:>rb_w$}  {:>rc_w$}",
            "rss_a",
            "rss_b",
            "rss_chg",
            ra_w = w.rss_a,
            rb_w = w.rss_b,
            rc_w = w.rss_change,
        )
        .expect("write to String is infallible");
    }
    if w.has_rewrite {
        write!(
            out,
            "  {:>rwa_w$}  {:>rwb_w$}",
            "rewrite_a",
            "rewrite_b",
            rwa_w = w.rewrite_a,
            rwb_w = w.rewrite_b,
        )
        .expect("write to String is infallible");
    }
    if w.has_blobs {
        write!(
            out,
            "  {:>ba_w$}  {:>bb_w$}",
            "blobs_a",
            "blobs_b",
            ba_w = w.blobs_a,
            bb_w = w.blobs_b,
        )
        .expect("write to String is infallible");
    }
}

fn append_compare_row(out: &mut String, pair: &ComparisonPair, w: &CompareWidths) {
    use std::fmt::Write;
    let cmd = &pair.key.command;
    let var = &pair.key.mode;
    let a_str = format_ms_or_dash(pair.a_wall);
    let b_str = format_ms_or_dash(pair.b_wall);
    let ch = format_change_pair(pair);
    write!(
        out,
        "{:<cmd_w$}  {:<var_w$}  {:<in_w$}  {:>a_w$}  {:>b_w$}  {:>ch_w$}",
        cmd,
        var,
        pair.input_display,
        a_str,
        b_str,
        ch,
        cmd_w = w.command,
        var_w = w.mode,
        in_w = w.input,
        a_w = w.col_a,
        b_w = w.col_b,
        ch_w = w.change,
    )
    .expect("write to String is infallible");
    if w.has_output {
        let oa = format_bytes_or_dash(pair.a_output_bytes);
        let ob = format_bytes_or_dash(pair.b_output_bytes);
        let oc = format_change_bytes(pair.a_output_bytes, pair.b_output_bytes);
        write!(
            out,
            "  {:>oa_w$}  {:>ob_w$}  {:>oc_w$}",
            oa,
            ob,
            oc,
            oa_w = w.output_a,
            ob_w = w.output_b,
            oc_w = w.output_change,
        )
        .expect("write to String is infallible");
    }
    if w.has_rss {
        let ra = format_rss_or_dash(pair.a_rss_mb);
        let rb = format_rss_or_dash(pair.b_rss_mb);
        let rc = format_change_rss(pair.a_rss_mb, pair.b_rss_mb);
        write!(
            out,
            "  {:>ra_w$}  {:>rb_w$}  {:>rc_w$}",
            ra,
            rb,
            rc,
            ra_w = w.rss_a,
            rb_w = w.rss_b,
            rc_w = w.rss_change,
        )
        .expect("write to String is infallible");
    }
    if w.has_rewrite {
        let rwa = format_pct_or_dash(pair.a_rewrite_pct);
        let rwb = format_pct_or_dash(pair.b_rewrite_pct);
        write!(
            out,
            "  {:>rwa_w$}  {:>rwb_w$}",
            rwa,
            rwb,
            rwa_w = w.rewrite_a,
            rwb_w = w.rewrite_b,
        )
        .expect("write to String is infallible");
    }
    if w.has_blobs {
        let ba = format_opt_str_or_dash(&pair.a_blobs);
        let bb = format_opt_str_or_dash(&pair.b_blobs);
        write!(
            out,
            "  {:>ba_w$}  {:>bb_w$}",
            ba,
            bb,
            ba_w = w.blobs_a,
            bb_w = w.blobs_b,
        )
        .expect("write to String is infallible");
    }
}

/// Render a wall for the A/B columns at its measured precision.
fn format_ms_or_dash(wall: Option<Wall>) -> String {
    wall.map_or_else(|| String::from("--"), format_wall)
}

/// Percentage change on the common scale supplied by each wall's precision.
fn format_change_pair(pair: &ComparisonPair) -> String {
    match (pair.a_wall, pair.b_wall) {
        (Some(a), Some(b)) => format_change(Some(a.ordering_key()), Some(b.ordering_key())),
        _ => format_change(None, None),
    }
}

fn format_change(a: Option<i128>, b: Option<i128>) -> String {
    match (a, b) {
        (Some(a), Some(b)) if a != 0 => {
            #[allow(clippy::cast_precision_loss)]
            let pct = ((b - a) as f64 / a as f64) * 100.0;
            if pct >= 0.0 {
                format!("+{pct:.1}%")
            } else {
                format!("{pct:.1}%")
            }
        }
        _ => String::from("--"),
    }
}

fn format_bytes_or_dash(bytes: Option<i64>) -> String {
    match bytes {
        Some(b) => {
            #[allow(clippy::cast_precision_loss)]
            let mb = b as f64 / (1024.0 * 1024.0);
            format!("{mb:.1} MB")
        }
        None => String::from("--"),
    }
}

fn format_change_bytes(a: Option<i64>, b: Option<i64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) if a != 0 => {
            #[allow(clippy::cast_precision_loss)]
            let pct = ((b - a) as f64 / a as f64) * 100.0;
            if pct >= 0.0 {
                format!("+{pct:.1}%")
            } else {
                format!("{pct:.1}%")
            }
        }
        _ => String::from("--"),
    }
}

fn format_rss_or_dash(mb: Option<f64>) -> String {
    match mb {
        Some(v) => format!("{v:.1} MB"),
        None => String::from("--"),
    }
}

fn format_change_rss(a: Option<f64>, b: Option<f64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) if a > 0.0 => {
            let pct = ((b - a) / a) * 100.0;
            if pct >= 0.0 {
                format!("+{pct:.1}%")
            } else {
                format!("{pct:.1}%")
            }
        }
        _ => String::from("--"),
    }
}

fn format_pct_or_dash(pct: Option<f64>) -> String {
    match pct {
        Some(v) => format!("{v:.1}%"),
        None => String::from("--"),
    }
}

fn format_opt_str_or_dash(s: &Option<String>) -> String {
    match s {
        Some(v) => v.clone(),
        None => String::from("--"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::super::super::KvPair;
    use super::super::super::StoredRow;
    use super::*;

    // -----------------------------------------------------------------------
    // Helper: build a StoredRow with sensible defaults, overriding key fields
    // -----------------------------------------------------------------------

    fn row(command: &str, variant: &str, input_file: &str, elapsed_ms: i64) -> StoredRow {
        super::super::fixture_row(command, variant, input_file, elapsed_ms)
    }

    // -----------------------------------------------------------------------
    // Pair-key identity keeps its components distinct.
    // -----------------------------------------------------------------------

    #[test]
    fn pair_key_preserves_all_components() {
        let key = pair_key(
            "read",
            "mmap",
            "denmark.osm.pbf",
            "pbfhogg/read/denmark/standard",
            "",
        );
        assert_eq!(key.command, "read");
        assert_eq!(key.mode, "mmap");
        assert_eq!(key.input_file, "denmark.osm.pbf");
        assert_eq!(key.arm_key, "pbfhogg/read/denmark/standard");
        assert_eq!(key.env_fingerprint, "");
    }

    #[test]
    fn pair_key_preserves_empty_components() {
        let key = pair_key("read", "", "", "", "");
        assert_eq!(key.command, "read");
        assert_eq!(key.mode, "");
        assert_eq!(key.input_file, "");
    }

    #[test]
    fn pair_key_allows_all_empty_components() {
        let key = pair_key("", "", "", "", "");
        assert_eq!(key.command, "");
        assert_eq!(key.mode, "");
        assert_eq!(key.input_file, "");
    }

    #[test]
    fn pair_key_distinguishes_by_arm_key() {
        // Same command/mode/input but different flags → different keys,
        // so --compare shows both runs instead of collapsing them.
        let k1 = pair_key(
            "apply-changes",
            "bench",
            "denmark.osm.pbf",
            "pbfhogg/apply-changes/direct-io=false",
            "",
        );
        let k2 = pair_key(
            "apply-changes",
            "bench",
            "denmark.osm.pbf",
            "pbfhogg/apply-changes/direct-io=true",
            "",
        );
        assert_ne!(k1, k2);
    }

    fn built(id: &str, wall: i64, provenance: &[(&str, &str)]) -> StoredRow {
        let mut r = row("render", "bench", "", wall);
        r.uuid = format!("{id}00000000");
        // Row ids follow the fixture id's digits, so recency is explicit.
        r.id = id.chars().filter(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0);
        r.kv = provenance
            .iter()
            .map(|(k, v)| KvPair::text(crate::db::KvKey::provenance(*k), *v))
            .collect();
        r
    }

    /// The latest rows are always the ones compared; differing provenance is
    /// named, and the most recent older pair with equal recorded provenance
    /// is offered beside it, never substituted.
    #[test]
    fn differing_provenance_is_named_and_an_equal_older_pair_offered() {
        let native = [("rustflags", "-Ctarget-cpu=native")];
        let generic = [("rustflags", "")];
        // One results.db numbers both sides: A rows 11, 10; B rows 21, 20.
        let a = [built("aaaa0011", 100, &native), built("aaaa0010", 120, &generic)];
        let b = [built("bbbb0021", 80, &generic), built("bbbb0020", 110, &generic)];
        let out = format_compare("A", &a, "B", &b, 10, &DatasetMatcher::empty());
        assert!(out.contains("build provenance differs: rustflags -Ctarget-cpu=native -> "), "{out}");
        assert!(
            out.contains("older pair with equal recorded build provenance: aaaa0010 vs bbbb0021"),
            "{out}"
        );
    }

    /// Among several equal-provenance older pairs, the one whose older member
    /// is newest wins, by row id - not by list position on each side.
    #[test]
    fn the_like_for_like_pair_is_the_most_recent_by_row_id() {
        let latest = [("rustflags", "-Ctarget-cpu=native")];
        let one = [("rustflags", "-Cone")];
        let two = [("rustflags", "-Ctwo")];
        // A rows ids 1000, 999, 998; B rows ids 100, 99. Equal-provenance
        // candidates: (999, 99) and (998, 100). By position the first wins
        // (A's second row); by recency the second does - its older member,
        // B's row 100, is newer than row 99.
        let a = [built("a1000", 1, &latest), built("a999", 1, &one), built("a998", 1, &two)];
        let b = [built("b100", 1, &two), built("b99", 1, &one)];
        let rows_a: Vec<&StoredRow> = a.iter().collect();
        let rows_b: Vec<&StoredRow> = b.iter().collect();
        let line = like_for_like(&rows_a, &rows_b).unwrap();
        assert!(line.contains("a9980000 vs b1000000"), "{line}");
    }

    /// History without provenance is unknown: never "equal", and on one side
    /// only it is said, not silently paired as a match.
    #[test]
    fn absent_provenance_is_unknown_not_equal() {
        let native = [("rustflags", "-Ctarget-cpu=native")];
        let generic = [("rustflags", "")];
        let a = [built("aaaa0001", 100, &native), built("aaaa0002", 120, &[])];
        let b = [built("bbbb0001", 80, &generic), built("bbbb0002", 110, &[])];
        let out = format_compare("A", &a, "B", &b, 10, &DatasetMatcher::empty());
        assert!(!out.contains("older pair"), "two unrecorded rows are not a match: {out}");
        let one_sided = format_compare("A", &[built("a", 1, &native)], "B", &[built("b", 1, &[])], 10, &DatasetMatcher::empty());
        assert!(one_sided.contains("build provenance: not recorded on B"), "{one_sided}");
        let neither = format_compare("A", &[built("a", 1, &[])], "B", &[built("b", 1, &[])], 10, &DatasetMatcher::empty());
        assert!(!neither.contains("provenance"), "{neither}");
    }

    #[test]
    fn workload_generations_do_not_pair_and_render_apart() {
        // One generation on each side, sharing their first eight hex: the
        // full digest splits the pair key, and the labels must still differ.
        let a = || {
            row(
                "method",
                "bench",
                "bench/method.lua@12345678aaaaaaaaaaaaaaaaaaaaaaaa",
                100,
            )
        };
        let b = || {
            row(
                "method",
                "bench",
                "bench/method.lua@12345678bbbbbbbbbbbbbbbbbbbbbbbb",
                90,
            )
        };
        let pairs = build_comparison_pairs(&[a()], &[b()], &DatasetMatcher::empty());
        assert_eq!(pairs.len(), 2, "two generations must not pair");
        let out = format_compare("aaa", &[a()], "bbb", &[b()], 10, &DatasetMatcher::empty());
        assert!(out.contains("method@12345678a"), "{out}");
        assert!(out.contains("method@12345678b"), "{out}");
    }

    #[test]
    fn compare_pairs_rows_with_matching_parsed_arm_keys() {
        // The parsed CLI excludes commit provenance before it produces the
        // arm key, so matching work pairs without parsing argv here.
        let mut a = row("render", "bench", "", 300);
        a.commit = "736e18c".parse().expect("fixture commit is valid");
        a.brokkr_args = String::from(r#"["brokkr","hotpath","--bench","1","--commit","736e18c"]"#);
        a.arm_key = Some(String::from("sluggrs/render/standard"));
        let mut b = row("render", "bench", "", 280);
        b.commit = "b2371d8".parse().expect("fixture commit is valid");
        b.brokkr_args = String::from(r#"["brokkr","hotpath","--bench","1"]"#);
        b.arm_key = Some(String::from("sluggrs/render/standard"));

        let pairs = build_comparison_pairs(&[a], &[b], &DatasetMatcher::empty());
        assert_eq!(
            pairs.len(),
            1,
            "retro and current rows should form one pair"
        );
        assert_eq!(pairs[0].a_wall.map(Wall::ms), Some(300));
        assert_eq!(pairs[0].b_wall.map(Wall::ms), Some(280));
    }

    #[test]
    fn pair_key_distinguishes_by_env_fingerprint() {
        // Same command/mode/input/flags but different captured env →
        // different keys, so env-gated A/B rows on the same commit stay
        // distinct in --compare instead of one silently winning.
        let k_off = pair_key("apply-changes", "bench", "dk.pbf", "args", "");
        let k_on = pair_key(
            "apply-changes",
            "bench",
            "dk.pbf",
            "args",
            "PBFHOGG_USE_NEW_PATH=1",
        );
        assert_ne!(k_off, k_on);
    }

    #[test]
    fn pair_key_keeps_tabs_inside_their_fields() {
        let key = pair_key("a\tb", "c", "d", "", "");
        assert_eq!(key.command, "a\tb");
        assert_eq!(key.mode, "c");
        assert_eq!(key.input_file, "d");
        assert_ne!(key, pair_key("a", "b", "c", "d", ""));
    }

    // -----------------------------------------------------------------------
    // build_comparison_pairs
    // -----------------------------------------------------------------------

    #[test]
    fn rows_pair_only_on_equal_known_builds() {
        use crate::build::{BuildKind, RustProfile};
        use crate::db::types::StoredProfile;
        let built = |profile: Option<StoredProfile>, id| {
            let mut r = row("corpus", "hotpath", "", 100);
            r.cargo_profile = profile;
            r.id = id;
            r
        };
        let corpus = || {
            Some(StoredProfile::Known(BuildKind::Cargo(
                RustProfile::parse("corpus").unwrap(),
            )))
        };
        let release = || Some(StoredProfile::Known(BuildKind::default()));
        let unknown = || Some(StoredProfile::Unknown("relesae".to_owned()));
        let m = DatasetMatcher::empty();

        assert_eq!(
            build_comparison_pairs(&[built(corpus(), 1)], &[built(corpus(), 2)], &m).len(),
            1
        );
        assert_eq!(
            build_comparison_pairs(&[built(release(), 1)], &[built(corpus(), 2)], &m).len(),
            2
        );
        assert_eq!(
            build_comparison_pairs(&[built(None, 1)], &[built(None, 2)], &m).len(),
            2
        );
        // The same unknown text on both sides still pairs with nothing.
        assert_eq!(
            build_comparison_pairs(&[built(unknown(), 1)], &[built(unknown(), 1)], &m).len(),
            2
        );
    }

    #[test]
    fn comparison_pairs_both_have_same_benchmark() {
        let a = vec![row("read", "mmap", "dk.pbf", 100)];
        let b = vec![row("read", "mmap", "dk.pbf", 90)];
        let pairs = build_comparison_pairs(&a, &b, &DatasetMatcher::empty());

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_wall.map(Wall::ms), Some(100));
        assert_eq!(pairs[0].b_wall.map(Wall::ms), Some(90));
    }

    #[test]
    fn comparison_pairs_a_only() {
        let a = vec![row("read", "mmap", "dk.pbf", 100)];
        let b: Vec<StoredRow> = vec![];
        let pairs = build_comparison_pairs(&a, &b, &DatasetMatcher::empty());

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_wall.map(Wall::ms), Some(100));
        assert_eq!(pairs[0].b_wall.map(Wall::ms), None);
    }

    #[test]
    fn comparison_pairs_b_only() {
        let a: Vec<StoredRow> = vec![];
        let b = vec![row("write", "", "out.pbf", 200)];
        let pairs = build_comparison_pairs(&a, &b, &DatasetMatcher::empty());

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_wall.map(Wall::ms), None);
        assert_eq!(pairs[0].b_wall.map(Wall::ms), Some(200));
    }

    #[test]
    fn comparison_pairs_deduplication_first_entry_wins() {
        // Two rows in A with the same key -- first one should win.
        let a = vec![
            row("read", "mmap", "dk.pbf", 100),
            row("read", "mmap", "dk.pbf", 999),
        ];
        let b = vec![
            row("read", "mmap", "dk.pbf", 50),
            row("read", "mmap", "dk.pbf", 888),
        ];
        let pairs = build_comparison_pairs(&a, &b, &DatasetMatcher::empty());

        assert_eq!(pairs.len(), 1);
        assert_eq!(
            pairs[0].a_wall.map(Wall::ms),
            Some(100),
            "first A entry should win, not 999"
        );
        assert_eq!(
            pairs[0].b_wall.map(Wall::ms),
            Some(50),
            "first B entry should win, not 888"
        );
    }

    #[test]
    fn comparison_pairs_ordering_a_first_then_b_new() {
        // A has benchmarks X and Y (in that order).
        // B has benchmarks Y and Z (in that order).
        // Expected key order: X, Y (from A), then Z (new from B).
        let a = vec![row("x-cmd", "", "", 10), row("y-cmd", "", "", 20)];
        let b = vec![row("y-cmd", "", "", 25), row("z-cmd", "", "", 30)];
        let pairs = build_comparison_pairs(&a, &b, &DatasetMatcher::empty());

        assert_eq!(pairs.len(), 3);
        let key_commands: Vec<String> = pairs.iter().map(|p| p.key.command.clone()).collect();
        assert_eq!(key_commands, vec!["x-cmd", "y-cmd", "z-cmd"]);

        // x-cmd: A-only
        assert_eq!(pairs[0].a_wall.map(Wall::ms), Some(10));
        assert_eq!(pairs[0].b_wall.map(Wall::ms), None);
        // y-cmd: both
        assert_eq!(pairs[1].a_wall.map(Wall::ms), Some(20));
        assert_eq!(pairs[1].b_wall.map(Wall::ms), Some(25));
        // z-cmd: B-only
        assert_eq!(pairs[2].a_wall.map(Wall::ms), None);
        assert_eq!(pairs[2].b_wall.map(Wall::ms), Some(30));
    }

    #[test]
    fn comparison_pairs_variant_and_input_matter() {
        // Same command but different variant/input should be separate pairs.
        let a = vec![
            row("read", "mmap", "dk.pbf", 100),
            row("read", "stdio", "dk.pbf", 200),
            row("read", "mmap", "se.pbf", 300),
        ];
        let b = vec![row("read", "mmap", "dk.pbf", 90)];
        let pairs = build_comparison_pairs(&a, &b, &DatasetMatcher::empty());

        assert_eq!(pairs.len(), 3);
        // Only the first pair should have both sides.
        assert!(pairs[0].a_wall.is_some() && pairs[0].b_wall.is_some());
        assert!(pairs[1].a_wall.is_some() && pairs[1].b_wall.is_none());
        assert!(pairs[2].a_wall.is_some() && pairs[2].b_wall.is_none());
    }

    #[test]
    fn comparison_pairs_empty_both_sides() {
        let pairs = build_comparison_pairs(&[], &[], &DatasetMatcher::empty());
        assert!(pairs.is_empty());
    }

    // -----------------------------------------------------------------------
    // format_change
    // -----------------------------------------------------------------------

    #[test]
    fn format_change_improvement() {
        // 100 -> 80 = -20%
        let result = format_change(Some(100), Some(80));
        assert_eq!(result, "-20.0%");
    }

    #[test]
    fn format_change_regression() {
        // 100 -> 130 = +30%
        let result = format_change(Some(100), Some(130));
        assert_eq!(result, "+30.0%");
    }

    #[test]
    fn format_change_same_value() {
        let result = format_change(Some(500), Some(500));
        assert_eq!(result, "+0.0%");
    }

    // -----------------------------------------------------------------------
    // format_counter_diff - the work-size context line
    // -----------------------------------------------------------------------

    fn counters(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn counter_diff_reports_every_counter_that_moved() {
        let a = counters(&[("parents", "100"), ("cells_evaluated", "5000")]);
        let b = counters(&[("parents", "100"), ("cells_evaluated", "4000")]);
        let out =
            format_counter_diff(&a, &b).expect("a moved counter is the context for the delta");
        assert!(out.contains("cells_evaluated 5000 -> 4000"), "{out}");
        // Unmoved counters are not news; listing them would bury the one that
        // moved.
        assert!(!out.contains("parents"), "{out}");
    }

    #[test]
    fn answer_diff_flags_only_two_recorded_different_answers() {
        let out = format_answer_diff(Some("1"), Some("0")).expect("different answers");
        assert!(out.contains("exit 1 -> 0"), "{out}");
        assert_eq!(format_answer_diff(Some("1"), Some("1")), None);
        assert_eq!(format_answer_diff(None, Some("1")), None);
        assert_eq!(format_answer_diff(None, None), None);
    }

    #[test]
    fn recorded_exit_reads_the_metadata_key_only() {
        let kv = vec![
            KvPair::int("exit_code", 9),
            KvPair::int(crate::db::KvKey::metadata("exit_code"), 1),
        ];
        assert_eq!(recorded_exit(&kv).as_deref(), Some("1"));
        assert_eq!(recorded_exit(&kv[..1]), None);
    }

    #[test]
    fn counter_diff_silent_when_nothing_moved() {
        let a = counters(&[("parents", "100"), ("cells_evaluated", "5000")]);
        assert_eq!(format_counter_diff(&a, &a), None);
    }

    #[test]
    fn counter_diff_reports_instrumentation_appearing_or_vanishing() {
        let present = counters(&[("prints", "7")]);
        let absent = counters(&[]);
        let out = format_counter_diff(&present, &absent).expect("vanishing is a finding");
        assert!(out.contains("(absent)"), "{out}");
        let out = format_counter_diff(&absent, &present).expect("appearing is a finding");
        assert!(out.contains("(absent)"), "{out}");
    }

    #[test]
    fn counter_diff_silent_when_neither_side_has_counters() {
        let empty = counters(&[]);
        assert_eq!(format_counter_diff(&empty, &empty), None);
    }

    #[test]
    fn collect_counters_excludes_provenance_pairs() {
        let kv = vec![
            KvPair::int("parents", 5),
            KvPair::text("meta.something", "x"),
            KvPair::text("env.MALLOC_CONF", "x"),
            KvPair::text("prev.command", "screen"),
        ];
        let got = collect_counters(&kv);
        // meta./env./prev. pairs legitimately differ between two runs without
        // the work differing, so they must not be comparable counters. `prev.*`
        // describes the preceding run and differs on nearly every pair.
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got.get("parents").map(String::as_str), Some("5"));
    }

    #[test]
    fn format_change_zero_baseline() {
        // a=0 falls through the guard `a != 0`, returns "--"
        let result = format_change(Some(0), Some(100));
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_missing_a() {
        let result = format_change(None, Some(100));
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_missing_b() {
        let result = format_change(Some(100), None);
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_both_missing() {
        let result = format_change(None, None);
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_large_regression() {
        // 1 -> 1001 = +100000%
        let result = format_change(Some(1), Some(1001));
        assert_eq!(result, "+100000.0%");
    }

    #[test]
    fn format_change_near_zero_result() {
        // 1000 -> 999: -0.1%
        let result = format_change(Some(1000), Some(999));
        assert_eq!(result, "-0.1%");
    }

    #[test]
    fn format_change_both_zero() {
        // a=0 hits the guard, returns "--"
        let result = format_change(Some(0), Some(0));
        assert_eq!(result, "--");
    }

    // -----------------------------------------------------------------------
    // compare with merge-specific columns
    // -----------------------------------------------------------------------

    #[test]
    fn comparison_pairs_carry_rewrite_and_blobs() {
        let mut a = row("bench merge", "buffered+zlib:6", "dk.pbf", 4500);
        a.kv = vec![
            KvPair::int("bytes_passthrough", 400_000_000),
            KvPair::int("bytes_rewritten", 40_000_000),
            KvPair::int("blobs_passthrough", 1200),
            KvPair::int("blobs_rewritten", 100),
        ];
        let mut b = row("bench merge", "buffered+zlib:6", "dk.pbf", 4200);
        b.kv = vec![
            KvPair::int("bytes_passthrough", 410_000_000),
            KvPair::int("bytes_rewritten", 35_000_000),
            KvPair::int("blobs_passthrough", 1210),
            KvPair::int("blobs_rewritten", 90),
        ];
        let pairs = build_comparison_pairs(&[a], &[b], &DatasetMatcher::empty());
        assert_eq!(pairs.len(), 1);

        let p = &pairs[0];
        let a_rw = p.a_rewrite_pct.unwrap();
        let b_rw = p.b_rewrite_pct.unwrap();
        // A: 40M / 440M ≈ 9.09%
        assert!((a_rw - 9.09).abs() < 0.1);
        // B: 35M / 445M ≈ 7.87%
        assert!((b_rw - 7.87).abs() < 0.1);

        assert_eq!(p.a_blobs.as_deref(), Some("1200pt/100rw"));
        assert_eq!(p.b_blobs.as_deref(), Some("1210pt/90rw"));
    }

    #[test]
    fn format_compare_surfaces_memory_delta() {
        // The motivating case: same command, same commit-under-test shape,
        // but the two runs saw very different amounts of free memory. The
        // compare view used to report only the wall delta, leaving the
        // environmental cause invisible.
        let mut a = row("read", "buffered", "planet.pbf", 19000);
        a.commit = "abc1234".parse().expect("fixture commit is valid");
        a.avail_memory_mb = Some(24780);
        let mut b = row("read", "buffered", "planet.pbf", 25500);
        b.commit = "def5678".parse().expect("fixture commit is valid");
        b.avail_memory_mb = Some(22221);

        let output = format_compare(
            "abc1234",
            &[a],
            "def5678",
            &[b],
            10,
            &DatasetMatcher::empty(),
        );
        assert!(
            output.contains("host:"),
            "differing host conditions should be annotated, got:\n{output}"
        );
        assert!(
            output.contains("24780 MB vs 22221 MB"),
            "both sides' memory should be shown, got:\n{output}"
        );
        assert!(
            output.contains("-2559 MB"),
            "the delta is the point - it should be spelled out, got:\n{output}"
        );
    }

    #[test]
    fn format_compare_quiet_when_host_matches() {
        let mut a = row("read", "buffered", "planet.pbf", 19000);
        a.commit = "abc1234".parse().expect("fixture commit is valid");
        a.avail_memory_mb = Some(24780);
        a.cpu_governor = String::from("performance");
        let mut b = row("read", "buffered", "planet.pbf", 19500);
        b.commit = "def5678".parse().expect("fixture commit is valid");
        b.avail_memory_mb = Some(24780);
        b.cpu_governor = String::from("performance");

        let output = format_compare(
            "abc1234",
            &[a],
            "def5678",
            &[b],
            10,
            &DatasetMatcher::empty(),
        );
        assert!(
            !output.contains("host:"),
            "identical host conditions should add no line, got:\n{output}"
        );
    }

    #[test]
    fn format_compare_flags_governor_change() {
        let mut a = row("read", "buffered", "planet.pbf", 19000);
        a.commit = "abc1234".parse().expect("fixture commit is valid");
        a.cpu_governor = String::from("performance");
        let mut b = row("read", "buffered", "planet.pbf", 25500);
        b.commit = "def5678".parse().expect("fixture commit is valid");
        b.cpu_governor = String::from("powersave");

        let output = format_compare(
            "abc1234",
            &[a],
            "def5678",
            &[b],
            10,
            &DatasetMatcher::empty(),
        );
        assert!(
            output.contains("governor performance vs powersave"),
            "a governor change invalidates the comparison and must be \
             visible, got:\n{output}"
        );
    }

    #[test]
    fn format_compare_shows_rewrite_columns() {
        let mut a = row("bench merge", "buffered+zlib:6", "dk.pbf", 4500);
        a.commit = "abc1234".parse().expect("fixture commit is valid");
        a.kv = vec![
            KvPair::int("bytes_passthrough", 920),
            KvPair::int("bytes_rewritten", 80),
            KvPair::int("blobs_passthrough", 100),
            KvPair::int("blobs_rewritten", 10),
        ];
        let mut b = row("bench merge", "buffered+zlib:6", "dk.pbf", 4200);
        b.commit = "def5678".parse().expect("fixture commit is valid");
        b.kv = vec![
            KvPair::int("bytes_passthrough", 900),
            KvPair::int("bytes_rewritten", 100),
            KvPair::int("blobs_passthrough", 95),
            KvPair::int("blobs_rewritten", 15),
        ];
        let output = format_compare(
            "abc1234",
            &[a],
            "def5678",
            &[b],
            10,
            &DatasetMatcher::empty(),
        );
        assert!(output.contains("rewrite_a"), "should have rewrite_a header");
        assert!(output.contains("rewrite_b"), "should have rewrite_b header");
        assert!(output.contains("blobs_a"), "should have blobs_a header");
        assert!(output.contains("blobs_b"), "should have blobs_b header");
        assert!(
            output.contains("8.0%"),
            "should show 8.0% rewrite ratio for A"
        );
        assert!(
            output.contains("10.0%"),
            "should show 10.0% rewrite ratio for B"
        );
        assert!(
            output.contains("100pt/10rw"),
            "should show blob counts for A"
        );
        assert!(
            output.contains("95pt/15rw"),
            "should show blob counts for B"
        );
    }

    #[test]
    fn format_compare_hides_rewrite_columns_when_absent() {
        let a = row("read", "mmap", "dk.pbf", 100);
        let b = row("read", "mmap", "dk.pbf", 90);
        let output = format_compare("aaa", &[a], "bbb", &[b], 10, &DatasetMatcher::empty());
        assert!(
            !output.contains("rewrite_a"),
            "no rewrite columns for non-merge"
        );
        assert!(!output.contains("blobs_a"), "no blob columns for non-merge");
    }
}
