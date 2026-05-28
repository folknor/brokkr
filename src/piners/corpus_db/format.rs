//! Console rendering for corpus query results.
//!
//! Formatter-on-rows, like `src/db/format`: each function takes already-loaded
//! row structs (no DB handle) and returns a column-aligned table string. The
//! generic [`grid`] primitive does the width computation; the typed renderers
//! just project their rows into cells.

use super::query::{DispositionRow, GateMissRow, RawTable, RunRow, TradeDiffRow, TrendRow};

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

/// The recent-runs table (bare `brokkr results` in piners).
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
                r.selector.clone(),
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
            "tier", "p90_en", "p90_ex", "p90_pnl", "signature", "error",
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
                ff(Some(t.our_pnl)),
                ff(t.tv_pnl),
            ]
        })
        .collect();
    grid(
        &[
            "our#", "tv#", "side", "Δentry_ts", "Δexit_ts", "Δentry_px", "Δexit_px", "our_pnl",
            "tv_pnl",
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
                ff(t.p90_exit),
            ]
        })
        .collect();
    grid(
        &[
            "run", "started_at", "disposition", "tier", "gate", "matched", "ours", "tv", "p90_ex",
        ],
        &cells,
    )
}

/// A raw `--where`/`--sql` result set.
pub fn raw_table(t: &RawTable) -> String {
    let headers: Vec<&str> = t.columns.iter().map(String::as_str).collect();
    grid(&headers, &t.rows)
}
