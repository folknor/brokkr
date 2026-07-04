//! Console rendering for corpus query results.
//!
//! Formatter-on-rows, like `src/db/format`: each function takes already-loaded
//! row structs (no DB handle) and returns a column-aligned table string. The
//! generic [`grid`] primitive does the width computation; the typed renderers
//! just project their rows into cells.

use serde_json::Value;

use super::query::{
    DispositionRow, GateMissRow, RawTable, RunRow, RuntimeRow, TradeDiffRow, TrendRow,
};

/// Render a header + rows as a left-aligned, space-padded grid. Empty rows
/// yield `"(none)"`.
fn grid(headers: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return "(none)".to_owned();
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }
    let mut out = String::new();
    let header_line: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:<width$}", h, width = widths[i]))
        .collect();
    out.push_str(header_line.join("  ").trim_end());
    out.push('\n');
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<width$}", c, width = widths.get(i).copied().unwrap_or(0)))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out.trim_end().to_owned()
}

/// Compact float formatting: trims trailing zeros, `-` for `None`. Exact
/// values are always available via `--sql`.
fn ff(v: Option<f64>) -> String {
    let Some(v) = v else {
        return "-".to_owned();
    };
    // {:.6} then trim trailing zeros/dot - yields "0" for 0.0, "0.08" for
    // 0.080000…, and "0" for sub-µ deltas (exact values live in --sql).
    let s = format!("{v:.6}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-0" {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn fi(v: Option<i64>) -> String {
    v.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

fn fs(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| "-".to_owned())
}

/// A boundary-discount count: `-` for zero (the common "nothing discounted"
/// case) so the non-zero values pop, ASCII so the byte-width grid stays
/// aligned.
fn bnd(v: i64) -> String {
    if v == 0 {
        "-".to_owned()
    } else {
        v.to_string()
    }
}

/// Compact the stored selector JSON to its *intent* for the runs table. The
/// selector persists the full resolved `ids` list so a run is reproducible,
/// but rendering all of them (231 for `--all`) wrecks the table - and the
/// probe count already has its own column, with the id list reachable via
/// `brokkr corpus-results <id>` or `--sql`. Show `all` / `kw=…` / `probe=…` (+`bless`)
/// instead. Falls back to the raw string if the JSON is an unexpected shape,
/// so nothing is silently blanked.
fn fmt_selector(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return raw.to_owned();
    };
    let mut parts: Vec<String> = Vec::new();
    if v.get("all").and_then(Value::as_bool) == Some(true) {
        parts.push("all".to_owned());
    }
    match v.get("probe") {
        // Current shape: an array of ids (repeatable `--probe`).
        Some(Value::Array(ps)) => {
            let names: Vec<&str> = ps.iter().filter_map(Value::as_str).collect();
            if !names.is_empty() {
                parts.push(format!("probe={}", names.join(",")));
            }
        }
        // Legacy shape (pre-repeatable `--probe`): a single string. Old
        // runs.db rows still carry it, so keep rendering them.
        Some(Value::String(p)) => parts.push(format!("probe={p}")),
        _ => {}
    }
    if let Some(kws) = v.get("keywords").and_then(Value::as_array) {
        let names: Vec<&str> = kws.iter().filter_map(Value::as_str).collect();
        if !names.is_empty() {
            parts.push(format!("kw={}", names.join(",")));
        }
    }
    if v.get("bless").and_then(Value::as_bool) == Some(true) {
        parts.push("bless".to_owned());
    }
    // Forwarded harness flags perturb harness behavior - render them so a
    // perturbed run is never mistaken for a clean one in the table.
    if let Some(extra) = v.get("harness_args").and_then(Value::as_array) {
        let flags: Vec<&str> = extra.iter().filter_map(Value::as_str).collect();
        if !flags.is_empty() {
            parts.push(format!("-- {}", flags.join(" ")));
        }
    }
    if parts.is_empty() {
        return raw.to_owned();
    }
    parts.join(" ")
}

/// The recent-runs table (bare `brokkr corpus-results`).
pub fn runs_table(rows: &[RunRow]) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.run_id.to_string(),
                r.started_at.clone(),
                r.result.clone(),
                if r.gated { "yes".to_owned() } else { "no".to_owned() },
                r.probe_count.to_string(),
                fi(r.harness_exit_code),
                r.fail_reason.clone().unwrap_or_default(),
                fmt_selector(&r.selector),
            ]
        })
        .collect();
    grid(
        &["run", "started_at", "result", "gated", "probes", "exit", "reason", "selector"],
        &cells,
    )
}

/// The per-probe disposition table (run detail / `--probe`).
pub fn dispositions_table(rows: &[DispositionRow]) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|d| {
            let sig = match (&d.sig_domain, &d.sig_dimension) {
                (Some(dom), Some(dim)) => format!("{dom}/{dim}"),
                (Some(dom), None) => dom.clone(),
                _ => "-".to_owned(),
            };
            vec![
                d.probe.clone(),
                d.outcome.clone(),
                d.disposition.clone(),
                fs(&d.expected),
                if d.gate_ok { "ok".to_owned() } else { "DEVIATES".to_owned() },
                d.matched.to_string(),
                d.ours_only.to_string(),
                d.tv_only.to_string(),
                bnd(d.boundary_ours),
                bnd(d.boundary_tv),
                fs(&d.count_tier),
                ff(d.p90_entry),
                ff(d.p90_exit),
                ff(d.p90_pnl),
                sig,
                d.error.clone().unwrap_or_default(),
            ]
        })
        .collect();
    grid(
        &[
            "probe", "outcome", "disposition", "expected", "gate", "matched", "ours", "tv",
            "b_ours", "b_tv", "tier", "p90_en", "p90_ex", "p90_pnl", "signature", "error",
        ],
        &cells,
    )
}

/// The gate-miss block (selected probes that emitted no line).
pub fn gate_misses_block(rows: &[GateMissRow]) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|m| vec![m.probe.clone(), fs(&m.expected), fs(&m.actual)])
        .collect();
    grid(&["probe", "expected", "actual"], &cells)
}

/// One probe's per-trade drill-down (`--probe`).
pub fn trade_diffs_table(rows: &[TradeDiffRow]) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|t| {
            vec![
                t.our_index.to_string(),
                t.tv_index.to_string(),
                fs(&t.our_side),
                fi(t.entry_ts_delta),
                fi(t.exit_ts_delta),
                ff(t.entry_price_delta),
                ff(t.exit_price_delta),
                ff(Some(t.our_qty)),
                ff(t.tv_entry_qty),
                ff(Some(t.our_pnl)),
                ff(t.tv_pnl),
            ]
        })
        .collect();
    grid(
        &[
            "our#", "tv#", "side", "Δentry_ts", "Δexit_ts", "Δentry_px", "Δexit_px", "our_qty",
            "tv_qty", "our_pnl", "tv_pnl",
        ],
        &cells,
    )
}

/// A probe's cross-run trend (`--trend`).
pub fn trend_table(rows: &[TrendRow]) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|t| {
            vec![
                t.run_id.to_string(),
                t.started_at.clone(),
                t.disposition.clone(),
                fs(&t.count_tier),
                if t.gate_ok { "ok".to_owned() } else { "DEVIATES".to_owned() },
                t.matched.to_string(),
                t.ours_only.to_string(),
                t.tv_only.to_string(),
                bnd(t.boundary_ours),
                bnd(t.boundary_tv),
                ff(t.p90_exit),
            ]
        })
        .collect();
    grid(
        &[
            "run", "started_at", "disposition", "tier", "gate", "matched", "ours", "tv", "b_ours",
            "b_tv", "p90_ex",
        ],
        &cells,
    )
}

/// A raw result set (`--where`/`--sql`, and the projected `--diffs` table).
pub fn raw_table(t: &RawTable) -> String {
    let headers: Vec<&str> = t.columns.iter().map(String::as_str).collect();
    grid(&headers, &t.rows)
}

/// A raw result set rendered vertically (psql `\x` style): one `column  value`
/// block per row, blank-line separated. Used for `--columns all`, where the 26
/// trade_diff columns won't fit a terminal row but a single-probe deep dive
/// wants them all.
pub fn raw_records(t: &RawTable) -> String {
    if t.rows.is_empty() {
        return "(none)".to_owned();
    }
    let width = t.columns.iter().map(String::len).max().unwrap_or(0);
    let mut out = String::new();
    for (i, row) in t.rows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("-- row {} --\n", i + 1));
        for (col, val) in t.columns.iter().zip(row) {
            out.push_str(&format!("{col:<width$}  {val}\n"));
        }
    }
    out.trim_end().to_owned()
}

/// The `--runtimes` view: each probe's most-recent runtime, slowest first, in
/// milliseconds - the unit the harness emits and the store keeps; rendering in
/// seconds flattened the sub-second majority of the corpus to `0.1`/`0.0`. A
/// *diagnostic* view for spotting the heavy probes (trim `bar_budget`, or
/// disable). `ceiling_ms` is the pre-run wall, shown for reference and to flag a
/// single probe that on its own clears it. The `Σ(shown)` footer is an explicit
/// **per-probe sum, NOT the run wall**: the harness overlaps probes, so this sum
/// runs several times the real wall - the ceiling estimates from brokkr's own
/// measured `run.wall_ms`, not from here (see `estimated_wall_ms`).
pub fn runtimes_table(rows: &[RuntimeRow], ceiling_ms: f64) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.probe.clone(),
                format!("{:.0}", r.runtime_ms),
                r.run_id.to_string(),
                if r.runtime_ms > ceiling_ms { "OVER".to_owned() } else { String::new() },
            ]
        })
        .collect();
    let mut out = grid(&["probe", "runtime_ms", "run", ""], &cells);
    if !rows.is_empty() {
        let sum_ms: f64 = rows.iter().map(|r| r.runtime_ms).sum();
        out.push_str(&format!(
            "\n\nΣ(shown) = {:.1}s (per-probe sum; probes overlap, so this is not \
             the run wall) · pre-run ceiling = {:.0}s",
            sum_ms / 1000.0,
            ceiling_ms / 1000.0,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::fmt_selector;

    #[test]
    fn compacts_all_selector_dropping_ids() {
        // The id list is the table-wrecker; `all` must stand in for it.
        let raw = r#"{"all":true,"bless":false,"ids":["a","b","c"],"keywords":[],"probe":null}"#;
        assert_eq!(fmt_selector(raw), "all");
    }

    #[test]
    fn compacts_keyword_and_probe_selectors() {
        let kw = r#"{"all":false,"bless":false,"ids":["a"],"keywords":["magnifier","bracket"],"probe":[]}"#;
        assert_eq!(fmt_selector(kw), "kw=magnifier,bracket");
        // Current shape: probe is an array (repeatable `--probe`).
        let probe = r#"{"all":false,"bless":false,"ids":["x","y"],"keywords":[],"probe":["x","y"]}"#;
        assert_eq!(fmt_selector(probe), "probe=x,y");
    }

    #[test]
    fn renders_legacy_string_probe_selector() {
        // Pre-repeatable runs persisted `probe` as a bare string; old
        // runs.db rows must still render.
        let probe = r#"{"all":false,"bless":false,"ids":["x"],"keywords":[],"probe":"x"}"#;
        assert_eq!(fmt_selector(probe), "probe=x");
    }

    #[test]
    fn appends_bless_and_falls_back_on_garbage() {
        let blessed = r#"{"all":true,"bless":true,"ids":[],"keywords":[],"probe":null}"#;
        assert_eq!(fmt_selector(blessed), "all bless");
        // Unparsable / unexpected shape is shown verbatim, never blanked.
        assert_eq!(fmt_selector("not json"), "not json");
        assert_eq!(fmt_selector("{}"), "{}");
    }

    #[test]
    fn renders_forwarded_harness_args() {
        // A perturbed run (forwarded harness flags) must be visibly distinct
        // from a clean run of the same selection. Empty array = clean.
        let perturbed = r#"{"all":false,"bless":false,"harness_args":["--scan-signal-extra","--scan-trade-chain"],"ids":["x"],"keywords":[],"probe":["x"]}"#;
        assert_eq!(
            fmt_selector(perturbed),
            "probe=x -- --scan-signal-extra --scan-trade-chain"
        );
        let clean = r#"{"all":false,"bless":false,"harness_args":[],"ids":["x"],"keywords":[],"probe":["x"]}"#;
        assert_eq!(fmt_selector(clean), "probe=x");
    }
}
