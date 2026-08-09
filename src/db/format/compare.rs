use super::super::{HotpathData, IDENTITY_COUNTERS_KEY, KvPair, StoredRow, TIMING_KEY};
use super::DatasetMatcher;
use super::table::{compute_rewrite_pct, find_output_bytes, format_blob_counts, format_input};

/// Format side-by-side comparison of two commits.
pub fn format_compare(
    commit_a: &str,
    rows_a: &[StoredRow],
    commit_b: &str,
    rows_b: &[StoredRow],
    top: usize,
    matcher: &DatasetMatcher,
) -> String {
    let pairs = build_comparison_pairs(rows_a, rows_b, matcher);
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
            let (cmd, var, _) = split_pair_key(&pair.key);
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
    key: String,
    a_ms: Option<i64>,
    b_ms: Option<i64>,
    /// Exact walls in microseconds, when both rows have them. The delta is
    /// computed from these in preference to `a_ms`/`b_ms`: on a single-digit
    /// millisecond benchmark, a percentage taken from rounded milliseconds
    /// is mostly quantisation noise.
    a_us: Option<i64>,
    b_us: Option<i64>,
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
    /// Work-size counters declared identity-bearing by the workload, and their
    /// values on each side. See [`format_counter_diff`].
    ///
    /// `None` means neither row opted into the counter contract, which is the
    /// case for every project that is not mogwai: their kv holds numbers too,
    /// but nothing declared what those numbers mean for comparability.
    identity_counters: Option<Vec<String>>,
    a_counters: std::collections::BTreeMap<String, String>,
    b_counters: std::collections::BTreeMap<String, String>,
    /// Which clock each side's `elapsed_ms` came from, when the row said.
    a_timing: Option<String>,
    b_timing: Option<String>,
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
    elapsed_ms: i64,
    elapsed_us: Option<i64>,
    hotpath: Option<HotpathData>,
    output_bytes: Option<i64>,
    peak_rss_mb: Option<f64>,
    rewrite_pct: Option<f64>,
    blobs: Option<String>,
    input_display: String,
    captured_env: std::collections::BTreeMap<String, String>,
    host: HostEnv,
    /// Names this row's workload declared identity-bearing, recorded WITH the
    /// run rather than read from today's config - so a later re-declaration
    /// cannot retroactively change what a historical comparison asserted.
    identity_counters: Option<Vec<String>>,
    /// Every non-`meta.` counter on the row, stringified for comparison.
    counters: std::collections::BTreeMap<String, String>,
    /// `meta.timing`, when the row recorded which clock it used.
    timing: Option<String>,
}

/// Split the recorded identity-counter declaration into names.
///
/// `None` when the key is absent entirely - the row is not a participant in the
/// counter contract. An EMPTY declaration is `Some(vec![])` and means something
/// different: the workload opted in and declared nothing identity-bearing, so
/// its counters are diffed but nothing about them is fatal.
fn parse_identity_counters(kv: &[KvPair]) -> Option<Vec<String>> {
    kv.iter().find(|p| p.key == IDENTITY_COUNTERS_KEY).map(|p| {
        p.value
            .to_string()
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// Collect a row's runtime counters, keyed by name.
///
/// `meta.`, `env.` and `prev.` pairs are provenance rather than measurement and
/// are excluded: they legitimately differ between two runs without meaning the
/// work did. `prev.*` in particular describes what the machine ran immediately
/// beforehand, so it differs on essentially every pair.
fn collect_counters(kv: &[KvPair]) -> std::collections::BTreeMap<String, String> {
    kv.iter()
        .filter(|p| {
            !p.key.starts_with("meta.")
                && !p.key.starts_with("env.")
                && !p.key.starts_with("prev.")
        })
        .map(|p| (p.key.clone(), p.value.to_string()))
        .collect()
}

/// Read the recorded clock (`meta.timing`) off a row.
fn parse_timing(kv: &[KvPair]) -> Option<String> {
    kv.iter()
        .find(|p| p.key == TIMING_KEY)
        .map(|p| p.value.to_string())
}

fn make_row_data(row: &StoredRow, matcher: &DatasetMatcher) -> RowData {
    RowData {
        elapsed_ms: row.elapsed_ms,
        elapsed_us: row.elapsed_us,
        hotpath: row.hotpath.clone(),
        output_bytes: find_output_bytes(&row.kv),
        peak_rss_mb: row.peak_rss_mb,
        rewrite_pct: compute_rewrite_pct(&row.kv),
        blobs: format_blob_counts(&row.kv),
        input_display: format_input(&row.input_file, row.input_mb, matcher),
        captured_env: row.captured_env.clone(),
        identity_counters: parse_identity_counters(&row.kv),
        counters: collect_counters(&row.kv),
        timing: parse_timing(&row.kv),
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

    let row_key = |row: &StoredRow| {
        pair_key(
            &row.command,
            &row.mode,
            &row.input_file,
            &normalize_brokkr_args(&row.brokkr_args),
            &row.env_fingerprint(),
        )
    };

    let mut keys: Vec<String> = Vec::new();
    let mut a_map: HashMap<String, RowData> = HashMap::new();
    let mut b_map: HashMap<String, RowData> = HashMap::new();

    for row in rows_a {
        let key = row_key(row);
        if let std::collections::hash_map::Entry::Vacant(e) = a_map.entry(key.clone()) {
            keys.push(key);
            e.insert(make_row_data(row, matcher));
        }
    }
    for row in rows_b {
        let key = row_key(row);
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
            // Either side's declaration will do: they are the same workload by
            // construction, and taking A's - falling back to B's - means a
            // comparison still checks the set when only one side recorded it,
            // which is exactly the case when the declaration was just added.
            let identity_counters = a
                .as_ref()
                .and_then(|r| r.identity_counters.clone())
                .filter(|v| !v.is_empty())
                .or_else(|| b.as_ref().and_then(|r| r.identity_counters.clone()))
                .or_else(|| a.as_ref().and_then(|r| r.identity_counters.clone()));
            let a_counters = a.as_ref().map(|r| r.counters.clone()).unwrap_or_default();
            let b_counters = b.as_ref().map(|r| r.counters.clone()).unwrap_or_default();
            let a_timing = a.as_ref().and_then(|r| r.timing.clone());
            let b_timing = b.as_ref().and_then(|r| r.timing.clone());
            ComparisonPair {
                key: k,
                a_ms: a.as_ref().map(|r| r.elapsed_ms),
                b_ms: b.as_ref().map(|r| r.elapsed_ms),
                a_us: a.as_ref().and_then(|r| r.elapsed_us),
                b_us: b.as_ref().and_then(|r| r.elapsed_us),
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
                identity_counters,
                a_counters,
                b_counters,
                a_timing,
                b_timing,
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
    if pair.a_ms.is_none() || pair.b_ms.is_none() {
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
    if let Some(annotation) = format_timing_diff(pair.a_timing.as_deref(), pair.b_timing.as_deref())
    {
        out.push_str(&annotation);
        out.push('\n');
    }
    // Both counter lines are for rows that opted into the contract. Every
    // project records numbers in its kv; only a declared workload asked for
    // them to be read as a statement about whether the two runs did the same
    // work, and reading an elivagar row that way would be inventing a claim.
    if let Some(identity) = &pair.identity_counters {
        if let Some(annotation) =
            format_counter_diff(identity, &pair.a_counters, &pair.b_counters)
        {
            out.push_str(&annotation);
            out.push('\n');
        }
        if let Some(annotation) =
            format_informational_counter_diff(identity, &pair.a_counters, &pair.b_counters)
        {
            out.push_str(&annotation);
            out.push('\n');
        }
    }
}

/// Format the clock annotation when the two sides recorded different timing
/// modes.
///
/// An external wall covers the whole invocation; a self-reported one covers
/// whatever window the target chose to measure. The difference between them can
/// be the entire delta - a corpus verification pass is minutes - and nothing
/// else on either row would reveal it, because both sides are just a count of
/// milliseconds. A workload switching clocks is supposed to become a new name;
/// this is what makes the forgotten rename visible instead of a fake speedup.
///
/// One side recording no clock is not a finding: rows predating `meta.timing`
/// exist, and they are all external, which is also the default.
fn format_timing_diff(a: Option<&str>, b: Option<&str>) -> Option<String> {
    let (a, b) = (a?, b?);
    if a == b {
        return None;
    }
    Some(format!(
        "    TIMING CHANGED: {a} -> {b} - the two walls measure different \
         windows and their delta is not a speedup"
    ))
}

/// Format the non-fatal counter line: every counter the workload did NOT
/// declare identity-bearing, that moved between the two sides.
///
/// Recorded and diffed, never fatal - the other half of the declared/undeclared
/// split. A counter like `cells_evaluated` is exactly what a real optimization
/// is supposed to move, so its movement is the finding rather than an error;
/// seeing it beside the wall delta is what turns "12% faster" into "12% faster
/// on 8% fewer cells". Suppressing it entirely, which is what shipping only the
/// identity check did, throws away the context that makes the delta readable.
fn format_informational_counter_diff(
    identity: &[String],
    a: &std::collections::BTreeMap<String, String>,
    b: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let mut names: std::collections::BTreeSet<&str> = a.keys().map(String::as_str).collect();
    names.extend(b.keys().map(String::as_str));

    let mut moved: Vec<String> = Vec::new();
    for name in names {
        // The declared ones have their own, louder line above.
        if identity.iter().any(|d| d == name) {
            continue;
        }
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

/// Format the work-size annotation when a declared identity-bearing counter
/// moved between the two sides.
///
/// The comparison this guards is the one a seeded benchmark makes possible: the
/// work is bit-identical run to run and across both sides of an A/B at the same
/// seed, so a counter that moved means the two rows describe different jobs and
/// the wall delta between them is not a speedup measurement. It catches the
/// classic optimization failure - accidentally doing less work and reading the
/// shorter wall as a win.
///
/// Only counters the workload DECLARED are checked. A blanket "every counter
/// must match" would fire on the first legitimate win, because doing less work
/// is what most optimization past the free-lane stage actually is; it would
/// then earn a bypass flag and be passed habitually until it meant nothing.
/// Undeclared counters are still recorded and still visible - just not fatal.
///
/// A counter present on one side and absent on the other is reported too: that
/// is what instrumentation appearing or disappearing mid-series looks like, and
/// it makes the pair no more comparable than a changed value does.
fn format_counter_diff(
    identity: &[String],
    a: &std::collections::BTreeMap<String, String>,
    b: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    if identity.is_empty() {
        return None;
    }
    let mut moved: Vec<String> = Vec::new();
    for name in identity {
        match (a.get(name), b.get(name)) {
            (Some(av), Some(bv)) if av == bv => {}
            (Some(av), Some(bv)) => moved.push(format!("{name} {av} -> {bv}")),
            // Absence on both sides is not a finding: the declaration may
            // simply be ahead of the instrumentation that will emit it.
            (None, None) => {}
            (Some(av), None) => moved.push(format!("{name} {av} -> (absent)")),
            (None, Some(bv)) => moved.push(format!("{name} (absent) -> {bv}")),
        }
    }
    if moved.is_empty() {
        return None;
    }
    Some(format!(
        "    WORK CHANGED: {} - the wall delta above is not a speedup",
        moved.join(", ")
    ))
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
    let mut keys: std::collections::BTreeSet<&str> =
        a.keys().map(String::as_str).collect();
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

/// Build the dedup/pair key for the compare view.
///
/// Post-v13 the axis (direct-io, compression, snapshot, index-type, …) lives
/// in `cli_args` / `brokkr_args` rather than in the `variant` column, so
/// `(command, mode, input_file)` alone would collapse axis-distinct runs
/// into one pair (silently hiding the rest). We include `brokkr_args` so
/// two runs of the same command with different flags show as separate
/// rows, and `env_fingerprint` so env-gated A/B rows on the same commit
/// don't collide either.
fn pair_key(
    command: &str,
    mode: &str,
    input_file: &str,
    brokkr_args: &str,
    env_fp: &str,
) -> String {
    format!("{command}\t{mode}\t{input_file}\t{brokkr_args}\t{env_fp}")
}

/// Drop the `brokkr_args` tokens that name *provenance or presentation*
/// rather than the benchmark arm, so they cannot enter the pair key.
///
/// `brokkr_args` is in the key on purpose: `--direct-io` and its kin define
/// genuinely different arms, and collapsing them into one row would silently
/// average two different benchmarks (see
/// `pair_key_distinguishes_by_brokkr_args`). Two flags invert that logic:
///
/// - `--commit REF` is the whole point of the comparison, not part of the
///   subject's identity. `--compare A B` asks for the same benchmark at two
///   commits, and `--commit` is *how* one side got its commit - so keying on
///   it makes the comparison axis part of the identity, and a `--commit` row
///   can never pair with the current-tree row it exists to be compared
///   against. That defeats the flag entirely.
/// - `--verbose`/`-v` only changes what is printed. It cannot move a wall.
///
/// Everything else stays. This is a display-level pairing heuristic, not a
/// correctness gate: a value that happens to be the literal string
/// `--commit` would consume the token after it, which no real invocation
/// produces.
fn normalize_brokkr_args(args: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skip_value = false;

    for token in args.split_whitespace() {
        if skip_value {
            skip_value = false;
            continue;
        }

        match token {
            // Space-separated form: the ref is the next token.
            "--commit" => skip_value = true,
            "--verbose" | "-v" => {}
            _ if token.starts_with("--commit=") => {}
            _ => out.push(token),
        }
    }

    out.join(" ")
}

fn split_pair_key(key: &str) -> (&str, &str, &str) {
    // splitn(5, …) - parts 4..=5 are brokkr_args / env_fingerprint, only
    // used for deduping. Callers only consume the first three
    // (command, mode, input_file).
    let mut parts = key.splitn(5, '\t');
    let cmd = parts.next().unwrap_or("");
    let var = parts.next().unwrap_or("");
    let input = parts.next().unwrap_or("");
    (cmd, var, input)
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
        let (cmd, var, _) = split_pair_key(&pair.key);
        w.command = w.command.max(cmd.len());
        w.mode = w.mode.max(var.len());
        w.input = w.input.max(pair.input_display.len());
        w.col_a = w.col_a.max(format_ms_or_dash(pair.a_ms, pair.a_us).len());
        w.col_b = w.col_b.max(format_ms_or_dash(pair.b_ms, pair.b_us).len());
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
    let (cmd, var, _) = split_pair_key(&pair.key);
    let a_str = format_ms_or_dash(pair.a_ms, pair.a_us);
    let b_str = format_ms_or_dash(pair.b_ms, pair.b_us);
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

/// Render a wall for the A/B columns.
///
/// Prefers the microsecond reading when the row has one, so a 6.847 ms
/// region does not print as `7 ms` next to a delta computed from the finer
/// value - the column and the change would visibly disagree.
fn format_ms_or_dash(ms: Option<i64>, us: Option<i64>) -> String {
    match (ms, us) {
        (_, Some(v)) => {
            #[allow(clippy::cast_precision_loss)]
            {
                format!("{:.3} ms", v as f64 / 1000.0)
            }
        }
        (Some(v), None) => format!("{v} ms"),
        (None, None) => String::from("--"),
    }
}

/// Percentage change for a pair, from microseconds when both sides have them.
fn format_change_pair(pair: &ComparisonPair) -> String {
    match (pair.a_us, pair.b_us) {
        (Some(a), Some(b)) => format_change(Some(a), Some(b)),
        _ => format_change(pair.a_ms, pair.b_ms),
    }
}

fn format_change(a_ms: Option<i64>, b_ms: Option<i64>) -> String {
    match (a_ms, b_ms) {
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
    use super::*;
    use super::super::super::KvPair;
    use super::super::super::StoredRow;

    // -----------------------------------------------------------------------
    // Helper: build a StoredRow with sensible defaults, overriding key fields
    // -----------------------------------------------------------------------

    fn row(command: &str, variant: &str, input_file: &str, elapsed_ms: i64) -> StoredRow {
        StoredRow {
            id: 0,
            timestamp: String::from("2026-03-01 00:00:00"),
            hostname: String::from("testhost"),
            commit: String::from("aabbccdd"),
            subject: String::from("test commit"),
            command: command.to_owned(),
            mode: variant.to_owned(),
            input_file: input_file.to_owned(),
            input_mb: None,
            elapsed_ms,
            elapsed_us: None,
            cargo_features: String::new(),
            cargo_profile: Some(crate::build::CargoProfile::Release),
            kernel: String::new(),
            cpu_governor: String::new(),
            avail_memory_mb: None,
            storage_notes: String::new(),
            peak_rss_mb: None,
            uuid: String::from("abcdef1234567890"),
            cli_args: String::new(),
            brokkr_args: String::new(),
            project: String::from("test"),
            stop_marker: String::new(),
            kv: vec![],
            captured_env: std::collections::BTreeMap::new(),
            iterations: Vec::new(),
            distribution: None,
            hotpath: None,
        }
    }

    // -----------------------------------------------------------------------
    // pair_key / split_pair_key roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn pair_key_roundtrip_normal() {
        let key = pair_key(
            "read",
            "mmap",
            "denmark.osm.pbf",
            "brokkr read --dataset denmark",
            "",
        );
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "read");
        assert_eq!(var, "mmap");
        assert_eq!(input, "denmark.osm.pbf");
    }

    #[test]
    fn pair_key_roundtrip_empty_fields() {
        let key = pair_key("read", "", "", "", "");
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "read");
        assert_eq!(var, "");
        assert_eq!(input, "");
    }

    #[test]
    fn pair_key_roundtrip_all_empty() {
        let key = pair_key("", "", "", "", "");
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "");
        assert_eq!(var, "");
        assert_eq!(input, "");
    }

    #[test]
    fn pair_key_distinguishes_by_brokkr_args() {
        // Same command/mode/input but different flags → different keys,
        // so --compare shows both runs instead of collapsing them.
        let k1 = pair_key(
            "apply-changes",
            "bench",
            "denmark.osm.pbf",
            "brokkr apply-changes --bench",
            "",
        );
        let k2 = pair_key(
            "apply-changes",
            "bench",
            "denmark.osm.pbf",
            "brokkr apply-changes --direct-io --bench",
            "",
        );
        assert_ne!(k1, k2);
    }

    #[test]
    fn normalize_strips_commit_so_retro_rows_pair() {
        // The motivating bug: a `--commit` run and a current-tree run of the
        // same benchmark produced different keys, so `--compare` printed two
        // separate rows with `--` in the change column instead of a delta.
        // `--commit` is the comparison axis, not part of the subject.
        let current = normalize_brokkr_args("brokkr hotpath --bench 1");
        let retro = normalize_brokkr_args("brokkr hotpath --bench 1 --commit 736e18c");
        assert_eq!(current, retro);
    }

    #[test]
    fn normalize_strips_commit_equals_form() {
        // clap accepts `--commit=REF` as readily as `--commit REF`.
        let spaced = normalize_brokkr_args("brokkr hotpath --bench 1 --commit 736e18c");
        let equals = normalize_brokkr_args("brokkr hotpath --bench 1 --commit=736e18c");
        assert_eq!(spaced, equals);
    }

    #[test]
    fn normalize_strips_verbose() {
        // `-v` changes only what is printed, so it must not split a pair.
        let quiet = normalize_brokkr_args("brokkr hotpath --bench 1");
        assert_eq!(normalize_brokkr_args("brokkr hotpath --bench 1 -v"), quiet);
        assert_eq!(
            normalize_brokkr_args("brokkr hotpath --bench 1 --verbose"),
            quiet
        );
    }

    #[test]
    fn normalize_keeps_arm_defining_flags() {
        // The guard on the fix above: normalization must not reach flags that
        // define a genuinely different benchmark arm.
        let plain = normalize_brokkr_args("brokkr apply-changes --bench");
        let direct = normalize_brokkr_args("brokkr apply-changes --direct-io --bench");
        assert_ne!(plain, direct);
        assert!(direct.contains("--direct-io"));
    }

    #[test]
    fn normalize_does_not_eat_the_token_after_an_unrelated_flag() {
        let out = normalize_brokkr_args("brokkr read --dataset denmark --bench 3");
        assert!(out.contains("denmark"), "got: {out}");
        assert!(out.contains("--bench 3"), "got: {out}");
    }

    #[test]
    fn compare_pairs_retro_and_current_rows() {
        // End-to-end at the pairing layer: same benchmark, one row recorded
        // via `--commit`, and they must land on one row with a delta.
        let mut a = row("render", "bench", "", 300);
        a.commit = String::from("736e18c");
        a.brokkr_args = String::from("brokkr hotpath --bench 1 --commit 736e18c");
        let mut b = row("render", "bench", "", 280);
        b.commit = String::from("b2371d8");
        b.brokkr_args = String::from("brokkr hotpath --bench 1");

        let pairs = build_comparison_pairs(&[a], &[b], &DatasetMatcher::empty());
        assert_eq!(pairs.len(), 1, "retro and current rows should form one pair");
        assert_eq!(pairs[0].a_ms, Some(300));
        assert_eq!(pairs[0].b_ms, Some(280));
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
    fn pair_key_tabs_in_values_still_bleed() {
        // splitn(5, '\t') means a tab inside the command field still
        // corrupts downstream fields. None of our inputs have tabs in
        // practice, but document the pitfall.
        let key = pair_key("a\tb", "c", "d", "", "");
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "a");
        assert_eq!(var, "b");
        assert_eq!(input, "c");
    }

    #[test]
    fn split_pair_key_no_tabs() {
        let (cmd, var, input) = split_pair_key("notabs");
        assert_eq!(cmd, "notabs");
        assert_eq!(var, "");
        assert_eq!(input, "");
    }

    // -----------------------------------------------------------------------
    // build_comparison_pairs
    // -----------------------------------------------------------------------

    #[test]
    fn comparison_pairs_both_have_same_benchmark() {
        let a = vec![row("read", "mmap", "dk.pbf", 100)];
        let b = vec![row("read", "mmap", "dk.pbf", 90)];
        let pairs = build_comparison_pairs(&a, &b, &DatasetMatcher::empty());

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_ms, Some(100));
        assert_eq!(pairs[0].b_ms, Some(90));
    }

    #[test]
    fn comparison_pairs_a_only() {
        let a = vec![row("read", "mmap", "dk.pbf", 100)];
        let b: Vec<StoredRow> = vec![];
        let pairs = build_comparison_pairs(&a, &b, &DatasetMatcher::empty());

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_ms, Some(100));
        assert_eq!(pairs[0].b_ms, None);
    }

    #[test]
    fn comparison_pairs_b_only() {
        let a: Vec<StoredRow> = vec![];
        let b = vec![row("write", "", "out.pbf", 200)];
        let pairs = build_comparison_pairs(&a, &b, &DatasetMatcher::empty());

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_ms, None);
        assert_eq!(pairs[0].b_ms, Some(200));
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
            pairs[0].a_ms,
            Some(100),
            "first A entry should win, not 999"
        );
        assert_eq!(pairs[0].b_ms, Some(50), "first B entry should win, not 888");
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
        let key_strings: Vec<String> = pairs
            .iter()
            .map(|p| split_pair_key(&p.key).0.to_owned())
            .collect();
        assert_eq!(key_strings, vec!["x-cmd", "y-cmd", "z-cmd"]);

        // x-cmd: A-only
        assert_eq!(pairs[0].a_ms, Some(10));
        assert_eq!(pairs[0].b_ms, None);
        // y-cmd: both
        assert_eq!(pairs[1].a_ms, Some(20));
        assert_eq!(pairs[1].b_ms, Some(25));
        // z-cmd: B-only
        assert_eq!(pairs[2].a_ms, None);
        assert_eq!(pairs[2].b_ms, Some(30));
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
        assert!(pairs[0].a_ms.is_some() && pairs[0].b_ms.is_some());
        assert!(pairs[1].a_ms.is_some() && pairs[1].b_ms.is_none());
        assert!(pairs[2].a_ms.is_some() && pairs[2].b_ms.is_none());
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
    // format_counter_diff - the work-size identity check
    // -----------------------------------------------------------------------

    fn counters(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn counter_diff_silent_when_declared_counters_match() {
        let a = counters(&[("parents", "100"), ("cells_evaluated", "5000")]);
        let b = counters(&[("parents", "100"), ("cells_evaluated", "4000")]);
        // cells_evaluated moved, but it is NOT declared identity-bearing -
        // doing fewer of those is what an optimization is supposed to look
        // like, and firing here is what would train everyone to ignore the line.
        let identity = vec!["parents".to_owned()];
        assert_eq!(format_counter_diff(&identity, &a, &b), None);
    }

    #[test]
    fn counter_diff_reports_a_moved_identity_counter() {
        let a = counters(&[("parents", "100")]);
        let b = counters(&[("parents", "90")]);
        let identity = vec!["parents".to_owned()];
        let out = format_counter_diff(&identity, &a, &b).expect("a moved counter must annotate");
        assert!(out.contains("parents 100 -> 90"), "{out}");
        // The line has to say what it means for the delta above it, or it
        // reads as trivia next to a number that looks like a win.
        assert!(out.contains("not a speedup"), "{out}");
    }

    #[test]
    fn counter_diff_reports_instrumentation_appearing_or_vanishing() {
        let identity = vec!["prints".to_owned()];
        let present = counters(&[("prints", "7")]);
        let absent = counters(&[]);
        let out = format_counter_diff(&identity, &present, &absent).expect("vanishing is a finding");
        assert!(out.contains("(absent)"), "{out}");
        let out = format_counter_diff(&identity, &absent, &present).expect("appearing is a finding");
        assert!(out.contains("(absent)"), "{out}");
    }

    #[test]
    fn counter_diff_silent_when_neither_side_emits_the_counter() {
        // The declaration may simply be ahead of the instrumentation that will
        // emit it; annotating every pair until then would be pure noise.
        let identity = vec!["prints".to_owned()];
        let empty = counters(&[]);
        assert_eq!(format_counter_diff(&identity, &empty, &empty), None);
    }

    #[test]
    fn counter_diff_silent_without_a_declaration() {
        // No declared set means no assertion: every other project's rows go
        // through this same path and must be unaffected.
        let a = counters(&[("parents", "100")]);
        let b = counters(&[("parents", "1")]);
        assert_eq!(format_counter_diff(&[], &a, &b), None);
    }

    #[test]
    fn identity_counters_round_trip_through_row_kv() {
        let kv = vec![KvPair::text(IDENTITY_COUNTERS_KEY, "parents, prints")];
        assert_eq!(
            parse_identity_counters(&kv),
            Some(vec!["parents".to_owned(), "prints".to_owned()])
        );
    }

    /// The three-state distinction the informational line depends on: absent
    /// (not a participant) is not the same as declared-empty (a participant
    /// that named nothing fatal).
    #[test]
    fn an_absent_declaration_is_not_an_empty_one() {
        assert_eq!(parse_identity_counters(&[]), None);
        let kv = vec![KvPair::text(IDENTITY_COUNTERS_KEY, "")];
        assert_eq!(parse_identity_counters(&kv), Some(Vec::new()));
    }

    #[test]
    fn informational_line_reports_undeclared_counters_that_moved() {
        let identity = vec!["parents".to_owned()];
        let a = counters(&[("parents", "100"), ("cells_evaluated", "5000")]);
        let b = counters(&[("parents", "100"), ("cells_evaluated", "4000")]);
        let out = format_informational_counter_diff(&identity, &a, &b)
            .expect("a moved undeclared counter is the context for the delta");
        assert!(out.contains("cells_evaluated 5000 -> 4000"), "{out}");
        // The declared one has its own louder line; repeating it here would
        // make the non-fatal line look like a second finding.
        assert!(!out.contains("parents"), "{out}");
    }

    #[test]
    fn informational_line_silent_when_nothing_moved() {
        let a = counters(&[("cells_evaluated", "5000")]);
        assert_eq!(format_informational_counter_diff(&[], &a, &a), None);
    }

    #[test]
    fn timing_diff_reports_a_switched_clock() {
        let out = format_timing_diff(Some("external"), Some("self_reported"))
            .expect("a switched clock must annotate");
        assert!(out.contains("TIMING CHANGED"), "{out}");
        assert_eq!(format_timing_diff(Some("external"), Some("external")), None);
    }

    #[test]
    fn timing_diff_silent_when_a_row_predates_the_key() {
        // Rows recorded before `meta.timing` existed are all external, which is
        // also the default - an absence is not a disagreement.
        assert_eq!(format_timing_diff(None, Some("external")), None);
        assert_eq!(format_timing_diff(Some("external"), None), None);
    }

    #[test]
    fn collect_counters_excludes_provenance_pairs() {
        let kv = vec![
            KvPair::int("parents", 5),
            KvPair::text(IDENTITY_COUNTERS_KEY, "parents"),
            KvPair::text("env.MALLOC_CONF", "x"),
        ];
        let got = collect_counters(&kv);
        // meta./env. pairs legitimately differ between two runs without the
        // work differing, so they must not be comparable counters.
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
        a.commit = String::from("abc1234");
        a.avail_memory_mb = Some(24780);
        let mut b = row("read", "buffered", "planet.pbf", 25500);
        b.commit = String::from("def5678");
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
        a.commit = String::from("abc1234");
        a.avail_memory_mb = Some(24780);
        a.cpu_governor = String::from("performance");
        let mut b = row("read", "buffered", "planet.pbf", 19500);
        b.commit = String::from("def5678");
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
        a.commit = String::from("abc1234");
        a.cpu_governor = String::from("performance");
        let mut b = row("read", "buffered", "planet.pbf", 25500);
        b.commit = String::from("def5678");
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
        a.commit = String::from("abc1234");
        a.kv = vec![
            KvPair::int("bytes_passthrough", 920),
            KvPair::int("bytes_rewritten", 80),
            KvPair::int("blobs_passthrough", 100),
            KvPair::int("blobs_rewritten", 10),
        ];
        let mut b = row("bench merge", "buffered+zlib:6", "dk.pbf", 4200);
        b.commit = String::from("def5678");
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
