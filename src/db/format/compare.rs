use super::super::{HotpathData, StoredRow};
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
        // Skip the env annotation when the pair is one-sided — the row
        // already shows `--` for the missing side's elapsed, so a
        // trailing "env: X=1 vs (unset)" would just duplicate that
        // signal and add noise on pairs where env isn't the
        // interesting axis at all.
        if pair.a_ms.is_some() && pair.b_ms.is_some()
            && let Some(annotation) = format_env_diff(&pair.a_env, &pair.b_env)
        {
            out.push_str(&annotation);
            out.push('\n');
        }
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
                format!("\n{label} — {commit_a} vs {commit_b}")
            } else {
                format!(
                    "\n{label} - {} — {commit_a} vs {commit_b}",
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
}

fn build_comparison_pairs(
    rows_a: &[StoredRow],
    rows_b: &[StoredRow],
    matcher: &DatasetMatcher,
) -> Vec<ComparisonPair> {
    use std::collections::HashMap;

    struct RowData {
        elapsed_ms: i64,
        hotpath: Option<HotpathData>,
        output_bytes: Option<i64>,
        peak_rss_mb: Option<f64>,
        rewrite_pct: Option<f64>,
        blobs: Option<String>,
        input_display: String,
        captured_env: std::collections::BTreeMap<String, String>,
    }

    let row_data = |row: &StoredRow| RowData {
        elapsed_ms: row.elapsed_ms,
        hotpath: row.hotpath.clone(),
        output_bytes: find_output_bytes(&row.kv),
        peak_rss_mb: row.peak_rss_mb,
        rewrite_pct: compute_rewrite_pct(&row.kv),
        blobs: format_blob_counts(&row.kv),
        input_display: format_input(&row.input_file, row.input_mb, matcher),
        captured_env: row.captured_env.clone(),
    };
    let row_key = |row: &StoredRow| {
        pair_key(
            &row.command,
            &row.mode,
            &row.input_file,
            &row.brokkr_args,
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
            e.insert(row_data(row));
        }
    }
    for row in rows_b {
        let key = row_key(row);
        if let std::collections::hash_map::Entry::Vacant(e) = b_map.entry(key.clone()) {
            if !a_map.contains_key(&key) {
                keys.push(key.clone());
            }
            e.insert(row_data(row));
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
            ComparisonPair {
                key: k,
                a_ms: a.as_ref().map(|r| r.elapsed_ms),
                b_ms: b.as_ref().map(|r| r.elapsed_ms),
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
            }
        })
        .collect()
}

/// Format a per-pair env annotation when A and B captured different
/// env sets. Returns `None` when the sets are identical (the common
/// case — captured_env is empty on >95% of historical rows). The
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

fn split_pair_key(key: &str) -> (&str, &str, &str) {
    // splitn(5, …) — parts 4..=5 are brokkr_args / env_fingerprint, only
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
        w.col_a = w.col_a.max(format_ms_or_dash(pair.a_ms).len());
        w.col_b = w.col_b.max(format_ms_or_dash(pair.b_ms).len());
        w.change = w.change.max(format_change(pair.a_ms, pair.b_ms).len());
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
    let a_str = format_ms_or_dash(pair.a_ms);
    let b_str = format_ms_or_dash(pair.b_ms);
    let ch = format_change(pair.a_ms, pair.b_ms);
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

fn format_ms_or_dash(ms: Option<i64>) -> String {
    match ms {
        Some(v) => format!("{v} ms"),
        None => String::from("--"),
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
