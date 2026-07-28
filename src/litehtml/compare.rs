//! Comparison logic for litehtml visual reference testing.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use crate::error::DevError;

#[allow(dead_code)]
pub(crate) struct PixelDiffResult {
    pub(crate) diff_pct: f64,
    pub(crate) total_pixels: u64,
    pub(crate) diff_pixels: u64,
}

#[allow(dead_code)]
pub(crate) struct ElementMatchResult {
    pub(crate) match_pct: f64,
    /// Scored elements: reference elements surviving the head/`br`/
    /// zero-height filters, whether or not the pipeline produced them.
    pub(crate) total_elements: usize,
    /// Scored elements whose geometry was within tolerance.
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

pub(crate) enum Status {
    Pass,
    /// Rendered fine, but there is no approved baseline to compare against.
    ///
    /// Distinct from [`Status::FailThreshold`] on purpose: "nobody has
    /// approved this yet" is the expected state of every newly registered
    /// snapshot, not a regression. Folding the two together made a first-ever
    /// run on a freshly wired project report "4 failed" for what was really
    /// "4 awaiting approval", with the same exit code as a real divergence.
    /// This status does not fail a run.
    NoBaseline,
    FailThreshold,
    Regression,
    ExpectedFail,
    Error,
}

impl Status {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::NoBaseline => "NO_BASELINE",
            Status::FailThreshold => "FAIL_THRESHOLD",
            Status::Regression => "REGRESSION",
            Status::ExpectedFail => "EXPECTED_FAIL",
            Status::Error => "ERROR",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Pixel comparison
// ---------------------------------------------------------------------------

const FUZZ_THRESHOLD: u8 = 13; // ~5% of 255

// 5% of 255 = 12.75, threshold rounds up to 13.
const _: () = {
    assert!(12u8 <= FUZZ_THRESHOLD);
    assert!(13u8 <= FUZZ_THRESHOLD);
    assert!(14u8 > FUZZ_THRESHOLD);
};

struct DecodedPng {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn decode_png(path: &Path) -> Result<DecodedPng, DevError> {
    let file = std::fs::File::open(path)
        .map_err(|e| DevError::Verify(format!("cannot open {}: {e}", path.display())))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .map_err(|e| DevError::Verify(format!("invalid PNG {}: {e}", path.display())))?;

    let info = reader.info();
    let width = info.width;
    let height = info.height;
    let color_type = info.color_type;
    let bit_depth = info.bit_depth;

    let buf_size = reader.output_buffer_size().ok_or_else(|| {
        DevError::Verify(format!(
            "cannot determine buffer size for {}",
            path.display()
        ))
    })?;
    let mut buf = vec![0u8; buf_size];
    let output_info = reader
        .next_frame(&mut buf)
        .map_err(|e| DevError::Verify(format!("PNG decode error {}: {e}", path.display())))?;
    buf.truncate(output_info.buffer_size());

    let rgba = to_rgba(&buf, color_type, bit_depth)?;

    Ok(DecodedPng {
        width,
        height,
        rgba,
    })
}

fn to_rgba(
    buf: &[u8],
    color_type: png::ColorType,
    bit_depth: png::BitDepth,
) -> Result<Vec<u8>, DevError> {
    if bit_depth != png::BitDepth::Eight {
        return Err(DevError::Verify(format!(
            "unsupported bit depth: {bit_depth:?}"
        )));
    }

    match color_type {
        png::ColorType::Rgba => Ok(buf.to_vec()),
        png::ColorType::Rgb => {
            let pixel_count = buf.len() / 3;
            let mut rgba = Vec::with_capacity(pixel_count * 4);
            let (chunks, _rem) = buf.as_chunks::<3>();
            for chunk in chunks {
                rgba.extend_from_slice(chunk);
                rgba.push(255);
            }
            Ok(rgba)
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity(buf.len() * 4);
            for &g in buf {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
            Ok(rgba)
        }
        png::ColorType::GrayscaleAlpha => {
            let pixel_count = buf.len() / 2;
            let mut rgba = Vec::with_capacity(pixel_count * 4);
            let (chunks, _rem) = buf.as_chunks::<2>();
            for chunk in chunks {
                let g = chunk[0];
                rgba.extend_from_slice(&[g, g, g, chunk[1]]);
            }
            Ok(rgba)
        }
        _ => Err(DevError::Verify(format!(
            "unsupported color type: {color_type:?}"
        ))),
    }
}

fn pad_to_size(img: &DecodedPng, target_w: u32, target_h: u32) -> Vec<u8> {
    if img.width == target_w && img.height == target_h {
        return img.rgba.clone();
    }

    let tw = target_w as usize;
    let th = target_h as usize;
    let mut out = vec![255u8; tw * th * 4]; // white fill

    let src_stride = img.width as usize * 4;
    let dst_stride = tw * 4;
    let copy_w = (img.width as usize).min(tw) * 4;

    for y in 0..(img.height as usize).min(th) {
        let src_start = y * src_stride;
        let dst_start = y * dst_stride;
        out[dst_start..dst_start + copy_w]
            .copy_from_slice(&img.rgba[src_start..src_start + copy_w]);
    }

    out
}

fn write_diff_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), DevError> {
    let file = std::fs::File::create(path).map_err(|e| {
        DevError::Verify(format!("cannot create diff image {}: {e}", path.display()))
    })?;
    let w = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| DevError::Verify(format!("PNG encode error: {e}")))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| DevError::Verify(format!("PNG write error: {e}")))?;
    Ok(())
}

pub(crate) fn compare_pixels(
    pipeline_png: &Path,
    reference_png: &Path,
    diff_output: &Path,
) -> Result<PixelDiffResult, DevError> {
    let pipeline = decode_png(pipeline_png)?;
    let reference = decode_png(reference_png)?;

    let max_w = pipeline.width.max(reference.width);
    let max_h = pipeline.height.max(reference.height);

    let pipe_rgba = pad_to_size(&pipeline, max_w, max_h);
    let ref_rgba = pad_to_size(&reference, max_w, max_h);

    let total_pixels = u64::from(max_w) * u64::from(max_h);
    let mut diff_pixels = 0u64;
    let mut diff_rgba = vec![0u8; pipe_rgba.len()];

    let pixel_count = (max_w as usize) * (max_h as usize);
    for i in 0..pixel_count {
        let base = i * 4;
        let pr = pipe_rgba[base];
        let pg = pipe_rgba[base + 1];
        let pb = pipe_rgba[base + 2];
        let rr = ref_rgba[base];
        let rg = ref_rgba[base + 1];
        let rb = ref_rgba[base + 2];

        let dr = pr.abs_diff(rr);
        let dg = pg.abs_diff(rg);
        let db = pb.abs_diff(rb);

        if dr > FUZZ_THRESHOLD || dg > FUZZ_THRESHOLD || db > FUZZ_THRESHOLD {
            diff_pixels += 1;
            diff_rgba[base] = 255;
            diff_rgba[base + 1] = 0;
            diff_rgba[base + 2] = 0;
            diff_rgba[base + 3] = 255;
        } else {
            // Dim the matching pixels
            diff_rgba[base] = rr / 3;
            diff_rgba[base + 1] = rg / 3;
            diff_rgba[base + 2] = rb / 3;
            diff_rgba[base + 3] = 255;
        }
    }

    write_diff_png(diff_output, max_w, max_h, &diff_rgba)?;

    let diff_pct = if total_pixels == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        {
            (diff_pixels as f64 / total_pixels as f64) * 100.0
        }
    };

    Ok(PixelDiffResult {
        diff_pct,
        total_pixels,
        diff_pixels,
    })
}

// ---------------------------------------------------------------------------
// Element comparison
// ---------------------------------------------------------------------------

const POS_TOLERANCE: f64 = 2.0;
const SIZE_TOLERANCE: f64 = 5.0;

/// Below Chrome's 0.1px dump quantum: "the reference says this element
/// has no height at all".
const ZERO_HEIGHT: f64 = 0.05;

#[derive(serde::Deserialize)]
struct LayoutElement {
    path: String,
    tag: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    w: Option<f64>,
    h: Option<f64>,
}

fn is_head_path(path: &str) -> bool {
    path.contains("head[") || path == "html>head"
}

/// Chrome emits `br` boxes; the pipeline folds line breaks into
/// rich-text leaves and never will emit them. Without this filter every
/// `br` lands in the chrome-only bucket and drags the score down for a
/// divergence that is pure convention. Matches on the dumped `tag` when
/// present, falling back to the path's leaf segment.
fn is_br(el: &LayoutElement) -> bool {
    if el.tag.as_deref() == Some("br") {
        return true;
    }

    let leaf = el
        .path
        .rsplit_once('>')
        .map_or(el.path.as_str(), |(_, leaf)| leaf);
    leaf == "br" || leaf.starts_with("br[")
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
        if ref_el.h.is_some_and(|h| h.abs() < ZERO_HEIGHT) {
            continue;
        }
        scored += 1;

        // Chrome-only elements count against the denominator but cannot
        // be offenders - there is no pipeline geometry to delta.
        let Some(pipe_el) = pipeline_by_path.get(path) else {
            continue;
        };

        let d = geometry_deltas(ref_el, pipe_el, &reference_by_path, &pipeline_by_path);
        if d.within_tolerance() {
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
        total_elements: scored,
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
    fn within_tolerance(&self) -> bool {
        self.dx.abs() <= POS_TOLERANCE
            && self.dy.abs() <= POS_TOLERANCE
            && self.dw.abs() <= SIZE_TOLERANCE
            && self.dh.abs() <= SIZE_TOLERANCE
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
    let frame = parent_path.and_then(|pp| {
        Some((*reference_by_path.get(pp)?, *pipeline_by_path.get(pp)?))
    });

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

const REGRESSION_TOLERANCE: f64 = 0.5;

pub(crate) fn determine_status(
    pixel_diff_pct: f64,
    element_match_pct: Option<f64>,
    pixel_threshold: f64,
    element_threshold: Option<f64>,
    expected_fail: bool,
    approved_pixel: Option<f64>,
    approved_element: Option<f64>,
) -> Status {
    let pixel_exceeds = pixel_diff_pct > pixel_threshold;

    let element_below = match (element_match_pct, element_threshold) {
        (Some(match_pct), Some(threshold)) => match_pct < threshold,
        _ => false,
    };

    let threshold_exceeded = pixel_exceeds || element_below;

    if expected_fail && threshold_exceeded {
        return Status::ExpectedFail;
    }

    if threshold_exceeded {
        return Status::FailThreshold;
    }

    if let Some(approved) = approved_pixel
        && pixel_diff_pct > approved + REGRESSION_TOLERANCE
    {
        return Status::Regression;
    }

    // The element ratchet: match pct is higher-is-better, so regression
    // is a *drop* below the approved value.
    if let (Some(current), Some(approved)) = (element_match_pct, approved_element)
        && current < approved - REGRESSION_TOLERANCE
    {
        return Status::Regression;
    }

    Status::Pass
}

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
    use super::*;

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
    fn br_detected_by_tag_or_path() {
        let mut by_tag = el("html>body[0]>p[0]>span[3]", 0.0, 0.0, 0.0, 16.0);
        by_tag.tag = Some("br".into());
        assert!(is_br(&by_tag));
        assert!(is_br(&el("html>body[0]>p[0]>br[2]", 0.0, 0.0, 0.0, 16.0)));
        assert!(!is_br(&el("html>body[0]>p[0]>b[0]", 0.0, 0.0, 0.0, 16.0)));
        // `brXX` tags must not match the `br[` prefix probe.
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
        let s = determine_status(5.0, Some(90.0), 10.0, Some(80.0), false, None, None);
        assert_eq!(s.as_str(), "PASS");
    }

    #[test]
    fn determine_status_fail_pixel() {
        let s = determine_status(15.0, Some(90.0), 10.0, Some(80.0), false, None, None);
        assert_eq!(s.as_str(), "FAIL_THRESHOLD");
    }

    #[test]
    fn determine_status_fail_element() {
        let s = determine_status(5.0, Some(70.0), 10.0, Some(80.0), false, None, None);
        assert_eq!(s.as_str(), "FAIL_THRESHOLD");
    }

    #[test]
    fn determine_status_expected_fail() {
        let s = determine_status(15.0, Some(70.0), 10.0, Some(80.0), true, None, None);
        assert_eq!(s.as_str(), "EXPECTED_FAIL");
    }

    #[test]
    fn determine_status_regression() {
        let s = determine_status(6.0, Some(90.0), 10.0, Some(80.0), false, Some(4.0), None);
        assert_eq!(s.as_str(), "REGRESSION");
    }

    #[test]
    fn determine_status_within_regression_tolerance() {
        let s = determine_status(4.3, Some(90.0), 10.0, Some(80.0), false, Some(4.0), None);
        assert_eq!(s.as_str(), "PASS");
    }

    #[test]
    fn determine_status_element_regression() {
        // Pixels fine, element match dropped below the approved value.
        let s = determine_status(1.0, Some(88.0), 10.0, Some(80.0), false, Some(1.0), Some(95.0));
        assert_eq!(s.as_str(), "REGRESSION");
    }

    #[test]
    fn determine_status_element_within_ratchet_tolerance() {
        let s = determine_status(1.0, Some(94.8), 10.0, Some(80.0), false, Some(1.0), Some(95.0));
        assert_eq!(s.as_str(), "PASS");
    }

    #[test]
    fn determine_status_element_improvement_passes() {
        let s = determine_status(1.0, Some(97.0), 10.0, Some(80.0), false, Some(1.0), Some(95.0));
        assert_eq!(s.as_str(), "PASS");
    }

    #[test]
    fn to_rgba_rgb_input() {
        let rgb = vec![255, 0, 0, 0, 255, 0];
        let rgba = to_rgba(&rgb, png::ColorType::Rgb, png::BitDepth::Eight).expect("convert");
        assert_eq!(rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn to_rgba_grayscale_input() {
        let gray = vec![128, 64];
        let rgba =
            to_rgba(&gray, png::ColorType::Grayscale, png::BitDepth::Eight).expect("convert");
        assert_eq!(rgba, vec![128, 128, 128, 255, 64, 64, 64, 255]);
    }

}
