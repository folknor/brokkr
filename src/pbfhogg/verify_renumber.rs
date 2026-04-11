//! Verify: renumber — `pbfhogg renumber` vs `osmium renumber`.
//!
//! Cross-validates pbfhogg's renumber output against osmium. Unlike the other
//! verify subcommands, a small number of diffs is expected and treated as a
//! PASS: pbfhogg's orphan-reference handling in relation members is a
//! documented semantic deviation (see pbfhogg `DEVIATIONS.md` /
//! `notes/renumber-planet-scale.md` section 5b).
//!
//! Fail conditions:
//! - element counts diverge between osmium and pbfhogg outputs
//! - any node or way diffs appear
//! - relation diff count exceeds `0.10 * total_relations` (sanity threshold)
//!
//! Scratch files are cleaned up on success and preserved on failure for
//! human review.

use std::fs;
use std::path::Path;

use super::verify::VerifyHarness;
use crate::error::DevError;
use crate::output::verify_msg;

/// Summary counts parsed from the `Summary: left=... right=... same=... different=...`
/// line produced by `pbfhogg diff -s`.
#[derive(Debug, Default)]
struct DiffSummary {
    left: u64,
    right: u64,
    same: u64,
    different: u64,
}

/// Per-element-type diff-block counts, derived from the `*n<id>` / `*w<id>` /
/// `*r<id>` block headers in `pbfhogg diff -c -v` output.
#[derive(Debug, Default)]
struct TypeCounts {
    nodes: u64,
    ways: u64,
    relations: u64,
}

/// Parse the `Summary:` line emitted by `pbfhogg diff -s`.
///
/// Example: `Summary: left=59152282 right=59152282 same=59151976 different=306`.
fn parse_summary(text: &str) -> Option<DiffSummary> {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("Summary:"))?;
    let mut s = DiffSummary::default();
    let mut saw_any = false;
    for field in line.split_whitespace() {
        if let Some(v) = field.strip_prefix("left=") {
            s.left = v.parse().ok()?;
            saw_any = true;
        } else if let Some(v) = field.strip_prefix("right=") {
            s.right = v.parse().ok()?;
            saw_any = true;
        } else if let Some(v) = field.strip_prefix("same=") {
            s.same = v.parse().ok()?;
            saw_any = true;
        } else if let Some(v) = field.strip_prefix("different=") {
            s.different = v.parse().ok()?;
            saw_any = true;
        }
    }
    if saw_any { Some(s) } else { None }
}

/// Scan the detailed diff output for per-element-type block headers.
///
/// Block headers look like `*n<id>`, `*w<id>`, `*r<id>` at the start of a
/// line (the `*` marks a changed element). Lines that don't match are
/// context, member lists, or tags and are ignored.
fn categorize_blocks(detail: &str) -> TypeCounts {
    let mut c = TypeCounts::default();
    for line in detail.lines() {
        let Some(rest) = line.strip_prefix('*') else {
            continue;
        };
        let mut chars = rest.chars();
        let Some(type_char) = chars.next() else {
            continue;
        };
        if !chars.next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        match type_char {
            'n' => c.nodes += 1,
            'w' => c.ways += 1,
            'r' => c.relations += 1,
            _ => {}
        }
    }
    c
}

/// Parse the total relation count from `pbfhogg inspect` output.
///
/// pbfhogg inspect prints the element breakdown across multiple lines:
///
/// ```text
/// Elements: 59,152,282 total
///   Nodes:        52,489,653  (25,411 tagged)
///   Ways:          6,616,526
///   Relations:       46,103
/// ```
///
/// With `--extended`, a later section prints an id-range line using the
/// same `Relations:` label (e.g. `Relations:  1 .. 46,103   (monotonic: yes)`),
/// so we anchor on the `Elements:` line first and then take the next
/// line whose first token is `Relations:`. The id-range line contains
/// `..` between the two numbers, and because the elements section comes
/// first in the output we never see it when anchoring forwards.
fn parse_total_relations(inspect_text: &str) -> Option<u64> {
    let mut in_elements = false;
    for line in inspect_text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Elements:") {
            in_elements = true;
            continue;
        }
        if !in_elements {
            continue;
        }
        // A blank line terminates the elements section.
        if trimmed.is_empty() {
            return None;
        }
        let mut toks = trimmed.split_whitespace();
        let Some(label) = toks.next() else {
            continue;
        };
        if label != "Relations:" {
            continue;
        }
        let num_tok = toks.next()?;
        let cleaned: String = num_tok.chars().filter(|c| *c != ',').collect();
        return cleaned.parse::<u64>().ok();
    }
    None
}

/// Format an integer with comma thousand separators.
fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Format a byte count as a short human-readable string.
fn fmt_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    #[allow(clippy::cast_precision_loss)]
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Cross-validate `pbfhogg renumber` against `osmium renumber`.
#[allow(clippy::too_many_lines)]
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    dataset: &str,
    mode: &str,
    start_id: Option<&str>,
    verbose: bool,
) -> Result<(), DevError> {
    let outdir = harness.subdir("renumber")?;

    verify_msg("--- renumber cross-validation ---");
    verify_msg(&format!("  dataset: {dataset}"));
    verify_msg(&format!("  input: {}", pbf.display()));
    verify_msg(&format!("  mode: {mode}"));
    if let Some(sid) = start_id {
        verify_msg(&format!("  start-id: {sid}"));
    }

    let pbf_str = pbf.display().to_string();

    // --- Output paths ---------------------------------------------------
    let osmium_out = outdir.join(format!("osmium-renumber-{dataset}.osm.pbf"));
    let pbfhogg_out = outdir.join(format!("pbfhogg-renumber-{dataset}-{mode}.osm.pbf"));
    let diff_log = outdir.join(format!("verify-renumber-{dataset}-{mode}-diff.txt"));
    let osmium_out_str = osmium_out.display().to_string();
    let pbfhogg_out_str = pbfhogg_out.display().to_string();

    // --- osmium renumber ------------------------------------------------
    let mut osmium_args: Vec<&str> = vec![
        "renumber",
        &pbf_str,
        "-o",
        &osmium_out_str,
        "--overwrite",
    ];
    if let Some(sid) = start_id {
        osmium_args.push("--start-id");
        osmium_args.push(sid);
    }
    let captured = harness.run_tool("osmium", &osmium_args)?;
    harness.check_exit(&captured, "osmium renumber")?;
    let osmium_elapsed = captured.elapsed;
    let osmium_size = fs::metadata(&osmium_out).map(|m| m.len()).unwrap_or(0);
    verify_msg(&format!(
        "  osmium renumber: {:.1}s, {} output",
        osmium_elapsed.as_secs_f64(),
        fmt_size(osmium_size),
    ));

    // --- pbfhogg renumber -----------------------------------------------
    let mut pbfhogg_args: Vec<&str> = vec![
        "renumber",
        &pbf_str,
        "-o",
        &pbfhogg_out_str,
        "--mode",
        mode,
    ];
    if let Some(sid) = start_id {
        pbfhogg_args.push("--start-id");
        pbfhogg_args.push(sid);
    }
    let captured = harness.run_pbfhogg(&pbfhogg_args)?;
    harness.check_exit(&captured, "pbfhogg renumber")?;
    let pbfhogg_elapsed = captured.elapsed;
    let pbfhogg_size = fs::metadata(&pbfhogg_out).map(|m| m.len()).unwrap_or(0);
    verify_msg(&format!(
        "  pbfhogg renumber: {:.1}s, {} output",
        pbfhogg_elapsed.as_secs_f64(),
        fmt_size(pbfhogg_size),
    ));

    // --- pbfhogg diff (summary + context + verbose) ---------------------
    // diff exits non-zero when mismatches exist, so do not check_exit.
    let captured = harness.run_pbfhogg(&[
        "diff",
        "-s",
        "-c",
        "-v",
        &osmium_out_str,
        &pbfhogg_out_str,
    ])?;
    let stdout = String::from_utf8_lossy(&captured.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&captured.stderr).into_owned();
    fs::write(&diff_log, format!("{stdout}{stderr}"))?;

    // The Summary line may be on stdout or stderr depending on pbfhogg's
    // version; try both.
    let summary = parse_summary(&stdout)
        .or_else(|| parse_summary(&stderr))
        .ok_or_else(|| {
            DevError::Verify(
                "renumber: could not parse 'Summary:' line from pbfhogg diff output".into(),
            )
        })?;

    // --- Total relation count (for threshold) ---------------------------
    // A parse failure here is a hard error rather than a silent fallback:
    // if we can't recover total_relations, the sanity-threshold check is
    // effectively disabled, which would let arbitrarily large relation-only
    // regressions sneak through as PASS.
    let inspect = harness.run_pbfhogg(&["inspect", &osmium_out_str])?;
    harness.check_exit(&inspect, "pbfhogg inspect")?;
    let inspect_text = String::from_utf8_lossy(&inspect.stdout);
    let total_relations = parse_total_relations(&inspect_text).ok_or_else(|| {
        DevError::Verify(
            "renumber: could not parse 'Relations:' count from pbfhogg inspect output — \
             has the inspect output format changed? Refusing to run the threshold check \
             with total_relations=0 (would let any relation-only regression PASS)."
                .into(),
        )
    })?;

    // --- Report ---------------------------------------------------------
    let total = summary.left.max(summary.right);
    #[allow(clippy::cast_precision_loss)]
    let pct_same = if total > 0 {
        (summary.same as f64 / total as f64) * 100.0
    } else {
        0.0
    };
    #[allow(clippy::cast_precision_loss)]
    let pct_diff = if total > 0 {
        (summary.different as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    if summary.left == summary.right {
        verify_msg(&format!(
            "  element counts: {} total on both sides (match)",
            fmt_num(summary.left),
        ));
    } else {
        verify_msg(&format!(
            "  element counts: osmium={} pbfhogg={} (MISMATCH)",
            fmt_num(summary.left),
            fmt_num(summary.right),
        ));
    }
    verify_msg(&format!(
        "  same: {} ({pct_same:.5}%)",
        fmt_num(summary.same),
    ));
    verify_msg(&format!(
        "  different: {} ({pct_diff:.5}%)",
        fmt_num(summary.different),
    ));

    // --- Categorize diff blocks by element type -------------------------
    let counts = categorize_blocks(&stdout);
    if summary.different > 0 {
        verify_msg(&format!(
            "    diff blocks: {} node(s), {} way(s), {} relation(s)",
            counts.nodes, counts.ways, counts.relations,
        ));
    }

    // --- Pass/fail decision ---------------------------------------------
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let threshold = ((total_relations as f64) * 0.10) as u64;

    let fail_reason: Option<String> = if summary.left != summary.right {
        Some(format!(
            "element counts diverge (osmium={}, pbfhogg={})",
            summary.left, summary.right,
        ))
    } else if counts.nodes > 0 || counts.ways > 0 {
        Some(format!(
            "node/way diffs detected (nodes={}, ways={})",
            counts.nodes, counts.ways,
        ))
    } else if summary.different > threshold {
        Some(format!(
            "relation diffs {} exceed sanity threshold {} (10% of {} relations)",
            summary.different, threshold, total_relations,
        ))
    } else {
        None
    };

    if verbose && summary.different > 0 {
        verify_msg(&format!("  diff log: {}", diff_log.display()));
        for line in stdout.lines().take(50) {
            verify_msg(&format!("    {line}"));
        }
    }

    match fail_reason {
        Some(reason) => {
            verify_msg(&format!("  result: FAIL ({reason})"));
            verify_msg(&format!("  diff log preserved: {}", diff_log.display()));
            Err(DevError::Verify(format!("renumber: {reason}")))
        }
        None => {
            if summary.different == 0 {
                verify_msg("  result: PASS (identical)");
            } else {
                verify_msg("  result: PASS (within expected delta)");
                verify_msg(&format!(
                    "    → {} relation-member diff(s) attributed to orphan-reference handling",
                    summary.different,
                ));
                verify_msg(
                    "    → documented deviation (see pbfhogg DEVIATIONS.md / notes/renumber-planet-scale.md)",
                );
            }
            // Clean up scratch files on success (best effort).
            drop(fs::remove_file(&osmium_out));
            drop(fs::remove_file(&pbfhogg_out));
            drop(fs::remove_file(&diff_log));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_summary_standard() {
        let text = "some noise\nSummary: left=59152282 right=59152282 same=59151976 different=306\ntrailing\n";
        let s = parse_summary(text).unwrap();
        assert_eq!(s.left, 59_152_282);
        assert_eq!(s.right, 59_152_282);
        assert_eq!(s.same, 59_151_976);
        assert_eq!(s.different, 306);
    }

    #[test]
    fn parse_summary_indented() {
        let text = "  Summary: left=1 right=2 same=3 different=4";
        let s = parse_summary(text).unwrap();
        assert_eq!(s.left, 1);
        assert_eq!(s.right, 2);
        assert_eq!(s.same, 3);
        assert_eq!(s.different, 4);
    }

    #[test]
    fn parse_summary_missing() {
        assert!(parse_summary("no summary here\n").is_none());
    }

    #[test]
    fn categorize_blocks_mixed() {
        let detail = "\
*n123 tag diff
  -v=a
  +v=b
*w456
  -member n1
  +member n2
*r789
  -member w10000000
  +member w20
*r790
  -member w10000001
  +member w21
";
        let c = categorize_blocks(detail);
        assert_eq!(c.nodes, 1);
        assert_eq!(c.ways, 1);
        assert_eq!(c.relations, 2);
    }

    #[test]
    fn categorize_blocks_ignores_non_headers() {
        let detail = "*not-a-header\n*n foo\n*nabc\nregular line\n";
        // "*not-a-header" — type char is 'n', next char is 'o' (not digit) → skipped.
        // "*n foo" — next char is ' ' → skipped.
        // "*nabc" — next char is 'a' → skipped.
        let c = categorize_blocks(detail);
        assert_eq!(c.nodes, 0);
        assert_eq!(c.ways, 0);
        assert_eq!(c.relations, 0);
    }

    #[test]
    fn parse_total_relations_standard() {
        // Actual pbfhogg inspect output (see pbfhogg/src/commands/inspect.rs
        // `print_elements`): multi-line, comma-separated, padded labels.
        let text = "\
File:     denmark-with-indexdata.osm.pbf (487 MB)
Features: Sort.Type_then_ID
Indexed:  yes
Blocks:   1234 total
Elements: 59,152,282 total
  Nodes:        52,489,653  (25,411 tagged)
  Ways:          6,616,526
  Relations:       46,103
Ordering: n -> w -> r (strict)
";
        assert_eq!(parse_total_relations(text), Some(46_103));
    }

    #[test]
    fn parse_total_relations_zero() {
        let text = "\
Elements: 59,105,679 total
  Nodes:        52,489,653
  Ways:          6,616,526
  Relations:            0
";
        assert_eq!(parse_total_relations(text), Some(0));
    }

    #[test]
    fn parse_total_relations_untagged_nodes_branch() {
        // Covers the `else` branch in print_elements where Nodes: has no
        // tagged-count suffix.
        let text = "\
Elements: 100 total
  Nodes:               50
  Ways:                30
  Relations:           20
";
        assert_eq!(parse_total_relations(text), Some(20));
    }

    #[test]
    fn parse_total_relations_ignores_id_range_section() {
        // With --extended, a later section emits an identically-labeled
        // "Relations:" line showing min..max. parse_total_relations must
        // lock onto the first (count) line and not the later (range) line.
        let text = "\
Elements: 59,152,282 total
  Nodes:        52,489,653
  Ways:          6,616,526
  Relations:       46,103

ID ranges:
  Nodes:        1 .. 11,000,000,000   (monotonic: yes)
  Ways:         1 .. 1,200,000,000    (monotonic: yes)
  Relations:    1 .. 17,000,000       (monotonic: yes)
";
        assert_eq!(parse_total_relations(text), Some(46_103));
    }

    #[test]
    fn parse_total_relations_missing() {
        assert_eq!(parse_total_relations("nothing relevant\n"), None);
    }

    #[test]
    fn parse_total_relations_elements_without_relations_line() {
        // Defensive: an "Elements:" line followed by a blank line
        // (no Relations: label at all) should return None, not hang or
        // mis-parse a later section's Relations: row.
        let text = "\
Elements: 100 total

  Relations:   46,103
";
        assert_eq!(parse_total_relations(text), None);
    }

    #[test]
    fn fmt_num_thousands() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(123), "123");
        assert_eq!(fmt_num(1234), "1,234");
        assert_eq!(fmt_num(59_152_282), "59,152,282");
    }

    #[test]
    fn fmt_size_units() {
        assert_eq!(fmt_size(500), "500 B");
        assert_eq!(fmt_size(2048), "2 KB");
        assert_eq!(fmt_size(5 * 1024 * 1024), "5 MB");
        assert_eq!(fmt_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
