//! Comparison logic for litehtml visual reference testing.

use std::collections::HashMap;
use std::path::Path;

use crate::error::DevError;
// Both visual subjects use the shared status thresholds and column formatter;
// keep project tests on this surface rather than growing adapter-local copies.
pub(crate) use crate::visual::compare::{compare_pixels, validate_artifact_id};
pub(crate) use crate::visual::status::{
    ApprovedScores, ComparisonThresholds, Status, StatusInput, StatusTally, determine_status,
    status_columns,
};

pub(crate) struct ElementMatchResult {
    pub(crate) match_pct: f64,
    /// Scored elements: reference elements surviving the head/`br`/
    /// zero-height filters, whether or not the pipeline produced them.
    #[cfg(test)]
    pub(crate) total_elements: usize,
    /// Scored elements whose geometry was within tolerance.
    #[cfg(test)]
    pub(crate) passing_elements: usize,
    /// Path-matched elements whose geometry fell outside tolerance,
    /// sorted worst-first by largest absolute delta.
    pub(crate) offenders: Vec<Offender>,
}

/// A path-matched element whose geometry fell outside tolerance. Deltas
/// are pipeline-minus-reference, in the same frame the comparison used:
/// parent-relative x/y when the parent resolved on both sides, absolute
/// otherwise. A coordinate present on only one side reports an infinite
/// delta.
pub(crate) struct Offender {
    pub(crate) path: String,
    pub(crate) dx: f64,
    pub(crate) dy: f64,
    pub(crate) dw: f64,
    pub(crate) dh: f64,
}

impl Offender {
    pub(crate) fn max_delta(&self) -> f64 {
        self.dx
            .abs()
            .max(self.dy.abs())
            .max(self.dw.abs())
            .max(self.dh.abs())
    }
}

// ---------------------------------------------------------------------------
// Element comparison
// ---------------------------------------------------------------------------

/// Geometry policy used by the scorer. Kept as an explicit input so tests can
/// exercise threshold boundaries without changing production defaults.
#[derive(Clone, Copy)]
struct ElementComparisonPolicy {
    position_tolerance: f64,
    size_tolerance: f64,
    zero_height_threshold: f64,
}

const DEFAULT_ELEMENT_POLICY: ElementComparisonPolicy = ElementComparisonPolicy {
    position_tolerance: 2.0,
    size_tolerance: 5.0,
    // Below Chrome's 0.1px dump quantum: "the reference says this element
    // has no height at all".
    zero_height_threshold: 0.05,
};

#[derive(serde::Deserialize)]
struct LayoutElement {
    path: String,
    tag: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    w: Option<f64>,
    h: Option<f64>,
}

/// Whether `path` is `head` or sits under it. Matches whole path segments:
/// a substring probe for `head[` also matched every `thead[N]`, silently
/// dropping whole table-header subtrees from scoring.
fn is_head_path(path: &str) -> bool {
    path.split('>')
        .any(|seg| seg == "head" || seg.starts_with("head["))
}

/// The element's tag: the dumped `tag` when present, else derived from
/// the path's leaf segment (`html>body[0]>span[2]` -> `span`).
fn leaf_tag(el: &LayoutElement) -> &str {
    if let Some(tag) = el.tag.as_deref() {
        return tag;
    }

    let leaf = el
        .path
        .rsplit_once('>')
        .map_or(el.path.as_str(), |(_, leaf)| leaf);
    leaf.split_once('[').map_or(leaf, |(tag, _)| tag)
}

/// Chrome emits `br` boxes; the pipeline folds line breaks into
/// rich-text leaves and never will emit them. Without this filter every
/// `br` lands in the chrome-only bucket and drags the score down for a
/// divergence that is pure convention.
fn is_br(el: &LayoutElement) -> bool {
    leaf_tag(el) == "br"
}

/// CSS-inline tags the pipeline folds into rich-text leaves when they
/// are plain inline. Mirror of litehtml-rs `is_inline_tag`
/// (src/style.rs) - keep in sync. `INLINE_ELEMENTS` in
/// `scripts/litehtml-prepare/prepare.js` must be a superset of this list.
///
/// Unlike `br`, this filter applies ONLY to the chrome-only bucket:
/// the pipeline *does* emit these elements when they are
/// display:inline-block (MJML nav links, buttons, social tables), and
/// those matched comparisons catch real bugs, so a path-matched inline
/// element is always scored.
const INLINE_TAGS: &[&str] = &[
    "span", "a", "strong", "em", "b", "i", "u", "small", "big", "sub", "sup", "code", "kbd",
    "samp", "var", "cite", "abbr", "mark", "del", "ins", "s", "q", "dfn", "ruby", "rt", "rp",
    "bdi", "bdo", "wbr", "time", "data", "output", "font",
];

fn is_inline_tag(tag: &str) -> bool {
    INLINE_TAGS.contains(&tag)
}

pub(crate) fn compare_elements(
    pipeline_json: &Path,
    reference_json: &Path,
) -> Result<ElementMatchResult, DevError> {
    let pipeline_text = std::fs::read_to_string(pipeline_json)
        .map_err(|e| DevError::Verify(format!("cannot read {}: {e}", pipeline_json.display())))?;
    let reference_text = std::fs::read_to_string(reference_json)
        .map_err(|e| DevError::Verify(format!("cannot read {}: {e}", reference_json.display())))?;

    let pipeline_elems: Vec<LayoutElement> = serde_json::from_str(&pipeline_text)
        .map_err(|e| DevError::Verify(format!("invalid JSON {}: {e}", pipeline_json.display())))?;
    let reference_elems: Vec<LayoutElement> = serde_json::from_str(&reference_text)
        .map_err(|e| DevError::Verify(format!("invalid JSON {}: {e}", reference_json.display())))?;

    Ok(compare_element_sets(&pipeline_elems, &reference_elems))
}

/// The comparison core, split from the file I/O so tests can feed
/// synthetic element sets directly.
fn compare_element_sets(
    pipeline_elems: &[LayoutElement],
    reference_elems: &[LayoutElement],
) -> ElementMatchResult {
    compare_element_sets_with_policy(pipeline_elems, reference_elems, DEFAULT_ELEMENT_POLICY)
}

fn compare_element_sets_with_policy(
    pipeline_elems: &[LayoutElement],
    reference_elems: &[LayoutElement],
    policy: ElementComparisonPolicy,
) -> ElementMatchResult {
    let pipeline_by_path: HashMap<&str, &LayoutElement> = pipeline_elems
        .iter()
        .filter(|e| !is_head_path(&e.path) && !is_br(e))
        .map(|e| (e.path.as_str(), e))
        .collect();

    let reference_by_path: HashMap<&str, &LayoutElement> = reference_elems
        .iter()
        .filter(|e| !is_head_path(&e.path) && !is_br(e))
        .map(|e| (e.path.as_str(), e))
        .collect();

    let mut scored = 0usize;
    let mut passing = 0usize;
    let mut offenders: Vec<Offender> = Vec::new();

    for (path, ref_el) in &reference_by_path {
        // Zero-height reference elements (empty tbody/tr) differ only in
        // an invisible convention: Chrome reports container width at
        // h=0, the pipeline reports 0x0. Skip them from scoring; they
        // stay in the map so they can still anchor a child's frame.
        if ref_el
            .h
            .is_some_and(|h| h.abs() < policy.zero_height_threshold)
        {
            continue;
        }

        let Some(pipe_el) = pipeline_by_path.get(path) else {
            // Chrome-only. A plain inline element here was folded into a
            // rich-text leaf - a representation choice, not a defect -
            // so it leaves the denominator. Anything else missing counts
            // against the score (but cannot be an offender: there is no
            // pipeline geometry to delta).
            if !is_inline_tag(leaf_tag(ref_el)) {
                scored += 1;
            }
            continue;
        };
        scored += 1;

        let d = geometry_deltas(ref_el, pipe_el, &reference_by_path, &pipeline_by_path);
        if d.within_tolerance(policy) {
            passing += 1;
        } else {
            offenders.push(Offender {
                path: (*path).to_owned(),
                dx: d.dx,
                dy: d.dy,
                dw: d.dw,
                dh: d.dh,
            });
        }
    }

    offenders.sort_by(|a, b| b.max_delta().total_cmp(&a.max_delta()));

    let match_pct = if scored == 0 {
        100.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        {
            (passing as f64 / scored as f64) * 100.0
        }
    };

    ElementMatchResult {
        match_pct,
        #[cfg(test)]
        total_elements: scored,
        #[cfg(test)]
        passing_elements: passing,
        offenders,
    }
}

struct GeometryDeltas {
    dx: f64,
    dy: f64,
    dw: f64,
    dh: f64,
}

impl GeometryDeltas {
    fn within_tolerance(&self, policy: ElementComparisonPolicy) -> bool {
        self.dx.abs() <= policy.position_tolerance
            && self.dy.abs() <= policy.position_tolerance
            && self.dw.abs() <= policy.size_tolerance
            && self.dh.abs() <= policy.size_tolerance
    }
}

/// Position deltas are parent-relative when the parent resolves on BOTH
/// sides: cumulative document drift (integer-vs-fractional line-height
/// rounding accumulates ~6px per 10,000px email) cancels out, and the
/// 2px tolerance means "this element sits wrong inside its parent", not
/// "everything below the drift point is wrong". Roots and elements whose
/// parent is missing from either map fall back to absolute coordinates.
/// Sizes are frame-independent and always compared absolutely.
fn geometry_deltas(
    ref_el: &LayoutElement,
    pipe_el: &LayoutElement,
    reference_by_path: &HashMap<&str, &LayoutElement>,
    pipeline_by_path: &HashMap<&str, &LayoutElement>,
) -> GeometryDeltas {
    let parent_path = ref_el.path.rsplit_once('>').map(|(parent, _)| parent);
    let frame =
        parent_path.and_then(|pp| Some((*reference_by_path.get(pp)?, *pipeline_by_path.get(pp)?)));

    let (dx, dy) = match frame {
        Some((ref_parent, pipe_parent)) => (
            delta(rel(ref_el.x, ref_parent.x), rel(pipe_el.x, pipe_parent.x)),
            delta(rel(ref_el.y, ref_parent.y), rel(pipe_el.y, pipe_parent.y)),
        ),
        None => (delta(ref_el.x, pipe_el.x), delta(ref_el.y, pipe_el.y)),
    };

    GeometryDeltas {
        dx,
        dy,
        dw: delta(ref_el.w, pipe_el.w),
        dh: delta(ref_el.h, pipe_el.h),
    }
}

/// Child coordinate relative to its parent; `None` if either is missing.
fn rel(child: Option<f64>, parent: Option<f64>) -> Option<f64> {
    Some(child? - parent?)
}

/// Pipeline minus reference. Both-missing is a vacuous match (0.0); a
/// value on only one side is an unconditional mismatch (infinite).
fn delta(reference: Option<f64>, pipeline: Option<f64>) -> f64 {
    match (reference, pipeline) {
        (Some(r), Some(p)) => p - r,
        (None, None) => 0.0,
        _ => f64::INFINITY,
    }
}

// ---------------------------------------------------------------------------
// Status determination
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
    use std::collections::HashSet;

    use super::*;

    const PREPARE_JS: &str = include_str!("../../scripts/litehtml-prepare/prepare.js");

    #[test]
    fn prepare_inline_tags_cover_every_scored_inline_tag() {
        let (_, declaration) = PREPARE_JS
            .split_once("const INLINE_ELEMENTS = new Set([")
            .expect("prepare.js declares its inline element set");
        let (list, _) = declaration
            .split_once("]);")
            .expect("prepare.js closes its inline element set");
        let prepare_tags: HashSet<_> = list
            .lines()
            .filter_map(|line| {
                line.trim()
                    .trim_end_matches(',')
                    .strip_prefix('"')
                    .and_then(|tag| tag.strip_suffix('"'))
            })
            .collect();

        for tag in INLINE_TAGS {
            assert!(
                prepare_tags.contains(tag),
                "prepare.js inline set is missing scorer tag {tag}"
            );
        }
        assert!(prepare_tags.contains("br"));
        assert!(prepare_tags.contains("img"));
    }

    fn el(path: &str, x: f64, y: f64, w: f64, h: f64) -> LayoutElement {
        LayoutElement {
            path: path.into(),
            tag: None,
            x: Some(x),
            y: Some(y),
            w: Some(w),
            h: Some(h),
        }
    }

    #[test]
    fn delta_both_present() {
        assert_eq!(delta(Some(10.0), Some(12.0)), 2.0);
    }

    #[test]
    fn delta_both_none_is_vacuous_match() {
        assert_eq!(delta(None, None), 0.0);
    }

    #[test]
    fn delta_one_sided_is_infinite() {
        assert!(delta(Some(10.0), None).is_infinite());
        assert!(delta(None, Some(10.0)).is_infinite());
    }

    #[test]
    fn head_paths_filtered() {
        assert!(is_head_path("html>head[0]>meta[0]"));
        assert!(is_head_path("html>head"));
        assert!(!is_head_path("html>body[0]>div[0]"));
    }

    #[test]
    fn thead_is_not_head() {
        assert!(!is_head_path("html>body[0]>table[0]>thead[0]"));
        assert!(!is_head_path("html>body[0]>table[0]>thead[0]>tr[0]>th[1]"));
        // And a thead subtree is actually scored.
        let reference = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]>table[0]>thead[0]", 0.0, 0.0, 600.0, 20.0),
        ];
        let r = compare_element_sets(&reference, &reference);
        assert_eq!(r.total_elements, 2);
    }

    #[test]
    fn br_detected_by_tag_or_path() {
        let mut by_tag = el("html>body[0]>p[0]>span[3]", 0.0, 0.0, 0.0, 16.0);
        by_tag.tag = Some("br".into());
        assert!(is_br(&by_tag));
        assert!(is_br(&el("html>body[0]>p[0]>br[2]", 0.0, 0.0, 0.0, 16.0)));
        assert!(!is_br(&el("html>body[0]>p[0]>b[0]", 0.0, 0.0, 0.0, 16.0)));
        // Path-derived tags must match `br` exactly, so a lookalike such as
        // `broken` stays in the comparison.
        assert!(!is_br(&el("html>body[0]>broken[0]", 0.0, 0.0, 0.0, 16.0)));
    }

    #[test]
    fn br_excluded_from_denominator() {
        // Reference has a br the pipeline will never emit; score should
        // still be 100%.
        let reference = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]>br[0]", 0.0, 20.0, 0.0, 16.0),
        ];
        let pipeline = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]", 0.0, 0.0, 800.0, 100.0),
        ];
        let r = compare_element_sets(&pipeline, &reference);
        assert_eq!(r.total_elements, 2);
        assert_eq!(r.match_pct, 100.0);
    }

    #[test]
    fn zero_height_reference_skipped() {
        // Empty tbody: Chrome says container width at h=0, pipeline says
        // 0x0. Must not be scored - but the parent stays usable.
        let reference = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]>table[0]>tbody[0]", 0.0, 50.0, 600.0, 0.0),
        ];
        let pipeline = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]>table[0]>tbody[0]", 0.0, 0.0, 0.0, 0.0),
        ];
        let r = compare_element_sets(&pipeline, &reference);
        assert_eq!(r.total_elements, 1);
        assert_eq!(r.match_pct, 100.0);
    }

    #[test]
    fn geometry_thresholds_are_injected_into_the_comparison() {
        let reference = vec![el("html", 0.0, 0.0, 800.0, 100.0)];
        let pipeline = vec![el("html", 1.0, 0.0, 803.0, 100.0)];

        let ordinary = compare_element_sets(&pipeline, &reference);
        assert_eq!(ordinary.passing_elements, 1);

        let strict = compare_element_sets_with_policy(
            &pipeline,
            &reference,
            ElementComparisonPolicy {
                position_tolerance: 0.5,
                size_tolerance: 1.0,
                zero_height_threshold: 0.0,
            },
        );
        assert_eq!(strict.passing_elements, 0);
        assert_eq!(strict.offenders.len(), 1);
        assert_eq!(strict.offenders[0].dx, 1.0);
        assert_eq!(strict.offenders[0].dw, 3.0);
    }

    #[test]
    fn status_columns_render_missing_values_and_delta_annotations() {
        let missing = status_columns(None, None, None, true);
        assert_eq!(missing.pixel, "\u{2014}");
        assert_eq!(missing.delta, "\u{2014}");
        assert_eq!(missing.status, "\u{2014}");
        assert_eq!(missing.annotation, "");

        let improved = status_columns(Some(Status::Pass), Some(1.0), Some(1.6), true);
        assert_eq!(improved.pixel, "1.0%");
        assert_eq!(improved.delta, "-0.6%");
        assert_eq!(improved.status, "PASS");
        assert_eq!(improved.annotation, " (improved)");

        let small_delta = status_columns(Some(Status::Pass), Some(1.0), Some(1.04), false);
        assert_eq!(small_delta.delta, "\u{2014}");
        assert_eq!(small_delta.annotation, "");
    }

    #[test]
    fn parent_relative_immune_to_document_drift() {
        // The pipeline's whole subtree sits 6px lower (cumulative
        // line-height drift). Absolute comparison would fail the child;
        // parent-relative must pass it.
        let reference = vec![
            el("html", 0.0, 0.0, 800.0, 10000.0),
            el("html>body[0]", 0.0, 0.0, 800.0, 10000.0),
            el("html>body[0]>div[0]", 0.0, 5000.0, 800.0, 100.0),
            el("html>body[0]>div[0]>p[0]", 10.0, 5010.0, 780.0, 20.0),
        ];
        let pipeline = vec![
            el("html", 0.0, 0.0, 800.0, 10004.0),
            el("html>body[0]", 0.0, 0.0, 800.0, 10004.0),
            el("html>body[0]>div[0]", 0.0, 5006.0, 800.0, 100.0),
            el("html>body[0]>div[0]>p[0]", 10.0, 5016.0, 780.0, 20.0),
        ];
        let r = compare_element_sets(&pipeline, &reference);
        // div is 6px off within body (a real local offset), but p sits
        // perfectly inside div. html/body/p pass; div is the offender.
        assert_eq!(r.total_elements, 4);
        assert_eq!(r.passing_elements, 3);
        assert_eq!(r.offenders.len(), 1);
        assert_eq!(r.offenders[0].path, "html>body[0]>div[0]");
        assert_eq!(r.offenders[0].dy, 6.0);
    }

    #[test]
    fn root_falls_back_to_absolute() {
        let reference = vec![el("html", 0.0, 0.0, 800.0, 100.0)];
        let pipeline = vec![el("html", 0.0, 10.0, 800.0, 100.0)];
        let r = compare_element_sets(&pipeline, &reference);
        assert_eq!(r.passing_elements, 0);
        assert_eq!(r.offenders[0].dy, 10.0);
    }

    #[test]
    fn offenders_sorted_worst_first() {
        let reference = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]>a[0]", 0.0, 10.0, 100.0, 20.0),
            el("html>body[0]>b[0]", 0.0, 40.0, 100.0, 20.0),
        ];
        let pipeline = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]>a[0]", 0.0, 14.0, 100.0, 20.0),
            el("html>body[0]>b[0]", 0.0, 40.0, 170.0, 20.0),
        ];
        let r = compare_element_sets(&pipeline, &reference);
        assert_eq!(r.offenders.len(), 2);
        assert_eq!(r.offenders[0].path, "html>body[0]>b[0]");
        assert_eq!(r.offenders[0].dw, 70.0);
        assert_eq!(r.offenders[1].path, "html>body[0]>a[0]");
        assert_eq!(r.offenders[1].dy, 4.0);
    }

    #[test]
    fn chrome_only_inline_leaves_denominator() {
        // Chrome emits the folded span/a; the pipeline folded them into
        // a rich-text leaf. Representation choice, not a defect.
        let reference = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]>p[0]>span[0]", 10.0, 10.0, 80.0, 16.0),
            el("html>body[0]>p[0]>a[0]", 90.0, 10.0, 40.0, 16.0),
        ];
        let pipeline = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]", 0.0, 0.0, 800.0, 100.0),
        ];
        let r = compare_element_sets(&pipeline, &reference);
        assert_eq!(r.total_elements, 2);
        assert_eq!(r.match_pct, 100.0);
    }

    #[test]
    fn matched_inline_still_scored() {
        // display:inline-block spans ARE emitted by the pipeline; when
        // both sides have one, the comparison must run - these catch
        // real bugs.
        let reference = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]>span[0]", 10.0, 10.0, 80.0, 16.0),
        ];
        let pipeline = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]>span[0]", 10.0, 10.0, 30.0, 16.0),
        ];
        let r = compare_element_sets(&pipeline, &reference);
        assert_eq!(r.total_elements, 2);
        assert_eq!(r.offenders.len(), 1);
        assert_eq!(r.offenders[0].path, "html>body[0]>span[0]");
        assert_eq!(r.offenders[0].dw, -50.0);
    }

    #[test]
    fn leaf_tag_falls_back_to_path() {
        assert_eq!(
            leaf_tag(&el("html>body[0]>span[2]", 0.0, 0.0, 0.0, 0.0)),
            "span"
        );
        assert_eq!(leaf_tag(&el("html", 0.0, 0.0, 0.0, 0.0)), "html");
        let mut tagged = el("html>body[0]>span[2]", 0.0, 0.0, 0.0, 0.0);
        tagged.tag = Some("a".into());
        assert_eq!(leaf_tag(&tagged), "a");
    }

    #[test]
    fn chrome_only_counts_against_score() {
        let reference = vec![
            el("html", 0.0, 0.0, 800.0, 100.0),
            el("html>body[0]", 0.0, 0.0, 800.0, 50.0),
        ];
        let pipeline = vec![el("html", 0.0, 0.0, 800.0, 100.0)];
        let r = compare_element_sets(&pipeline, &reference);
        assert_eq!(r.total_elements, 2);
        assert_eq!(r.passing_elements, 1);
        assert_eq!(r.match_pct, 50.0);
        // Not an offender - no pipeline geometry to report.
        assert!(r.offenders.is_empty());
    }

    #[test]
    fn determine_status_pass() {
        let s = determine_status(&StatusInput {
            pixel_diff_pct: 5.0,
            element_match_pct: Some(90.0),
            thresholds: ComparisonThresholds {
                pixel: 10.0,
                element: Some(80.0),
            },
            expected_fail: false,
            approved: ApprovedScores {
                pixel: None,
                element: None,
            },
        });
        assert_eq!(s.as_str(), "PASS");
    }

    #[test]
    fn determine_status_fail_pixel() {
        let s = determine_status(&StatusInput {
            pixel_diff_pct: 15.0,
            element_match_pct: Some(90.0),
            thresholds: ComparisonThresholds {
                pixel: 10.0,
                element: Some(80.0),
            },
            expected_fail: false,
            approved: ApprovedScores {
                pixel: None,
                element: None,
            },
        });
        assert_eq!(s.as_str(), "FAIL_THRESHOLD");
    }

    #[test]
    fn determine_status_fail_element() {
        let s = determine_status(&StatusInput {
            pixel_diff_pct: 5.0,
            element_match_pct: Some(70.0),
            thresholds: ComparisonThresholds {
                pixel: 10.0,
                element: Some(80.0),
            },
            expected_fail: false,
            approved: ApprovedScores {
                pixel: None,
                element: None,
            },
        });
        assert_eq!(s.as_str(), "FAIL_THRESHOLD");
    }

    #[test]
    fn determine_status_expected_fail() {
        let s = determine_status(&StatusInput {
            pixel_diff_pct: 15.0,
            element_match_pct: Some(70.0),
            thresholds: ComparisonThresholds {
                pixel: 10.0,
                element: Some(80.0),
            },
            expected_fail: true,
            approved: ApprovedScores {
                pixel: None,
                element: None,
            },
        });
        assert_eq!(s.as_str(), "EXPECTED_FAIL");
    }

    #[test]
    fn determine_status_regression() {
        let s = determine_status(&StatusInput {
            pixel_diff_pct: 6.0,
            element_match_pct: Some(90.0),
            thresholds: ComparisonThresholds {
                pixel: 10.0,
                element: Some(80.0),
            },
            expected_fail: false,
            approved: ApprovedScores {
                pixel: Some(4.0),
                element: None,
            },
        });
        assert_eq!(s.as_str(), "REGRESSION");
    }

    #[test]
    fn determine_status_within_regression_tolerance() {
        let s = determine_status(&StatusInput {
            pixel_diff_pct: 4.3,
            element_match_pct: Some(90.0),
            thresholds: ComparisonThresholds {
                pixel: 10.0,
                element: Some(80.0),
            },
            expected_fail: false,
            approved: ApprovedScores {
                pixel: Some(4.0),
                element: None,
            },
        });
        assert_eq!(s.as_str(), "PASS");
    }

    #[test]
    fn determine_status_element_regression() {
        // Pixels fine, element match dropped below the approved value.
        let s = determine_status(&StatusInput {
            pixel_diff_pct: 1.0,
            element_match_pct: Some(88.0),
            thresholds: ComparisonThresholds {
                pixel: 10.0,
                element: Some(80.0),
            },
            expected_fail: false,
            approved: ApprovedScores {
                pixel: Some(1.0),
                element: Some(95.0),
            },
        });
        assert_eq!(s.as_str(), "REGRESSION");
    }

    #[test]
    fn determine_status_element_within_ratchet_tolerance() {
        let s = determine_status(&StatusInput {
            pixel_diff_pct: 1.0,
            element_match_pct: Some(94.8),
            thresholds: ComparisonThresholds {
                pixel: 10.0,
                element: Some(80.0),
            },
            expected_fail: false,
            approved: ApprovedScores {
                pixel: Some(1.0),
                element: Some(95.0),
            },
        });
        assert_eq!(s.as_str(), "PASS");
    }

    #[test]
    fn determine_status_element_improvement_passes() {
        let s = determine_status(&StatusInput {
            pixel_diff_pct: 1.0,
            element_match_pct: Some(97.0),
            thresholds: ComparisonThresholds {
                pixel: 10.0,
                element: Some(80.0),
            },
            expected_fail: false,
            approved: ApprovedScores {
                pixel: Some(1.0),
                element: Some(95.0),
            },
        });
        assert_eq!(s.as_str(), "PASS");
    }
}
