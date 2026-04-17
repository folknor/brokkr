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
    pub(crate) total_elements: usize,
    pub(crate) matched_elements: usize,
}

pub(crate) enum Status {
    Pass,
    FailThreshold,
    Regression,
    ExpectedFail,
    Error,
}

impl Status {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Status::Pass => "PASS",
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
            for chunk in buf.chunks_exact(3) {
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
            for chunk in buf.chunks_exact(2) {
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

#[derive(serde::Deserialize)]
struct LayoutElement {
    path: String,
    #[allow(dead_code)]
    tag: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    w: Option<f64>,
    h: Option<f64>,
}

fn is_head_path(path: &str) -> bool {
    path.contains("head[") || path == "html>head"
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

    let pipeline_by_path: HashMap<&str, &LayoutElement> = pipeline_elems
        .iter()
        .filter(|e| !is_head_path(&e.path))
        .map(|e| (e.path.as_str(), e))
        .collect();

    let reference_by_path: HashMap<&str, &LayoutElement> = reference_elems
        .iter()
        .filter(|e| !is_head_path(&e.path))
        .map(|e| (e.path.as_str(), e))
        .collect();

    let mut matched = 0usize;
    let mut exact = 0usize;
    let mut chrome_only = 0usize;

    for (path, ref_el) in &reference_by_path {
        if let Some(pipe_el) = pipeline_by_path.get(path) {
            matched += 1;
            if geometry_matches(ref_el, pipe_el) {
                exact += 1;
            }
        } else {
            chrome_only += 1;
        }
    }

    let denominator = matched + chrome_only;
    let match_pct = if denominator == 0 {
        100.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        {
            (exact as f64 / denominator as f64) * 100.0
        }
    };

    Ok(ElementMatchResult {
        match_pct,
        total_elements: denominator,
        matched_elements: exact,
    })
}

fn geometry_matches(a: &LayoutElement, b: &LayoutElement) -> bool {
    within_tol(a.x, b.x, POS_TOLERANCE)
        && within_tol(a.y, b.y, POS_TOLERANCE)
        && within_tol(a.w, b.w, SIZE_TOLERANCE)
        && within_tol(a.h, b.h, SIZE_TOLERANCE)
}

fn within_tol(a: Option<f64>, b: Option<f64>, tol: f64) -> bool {
    match (a, b) {
        (Some(va), Some(vb)) => (va - vb).abs() <= tol,
        (None, None) => true,
        _ => false,
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

    #[test]
    fn within_tol_both_present_within() {
        assert!(within_tol(Some(10.0), Some(12.0), 2.0));
    }

    #[test]
    fn within_tol_both_present_outside() {
        assert!(!within_tol(Some(10.0), Some(13.0), 2.0));
    }

    #[test]
    fn within_tol_both_none() {
        assert!(within_tol(None, None, 5.0));
    }

    #[test]
    fn within_tol_one_none() {
        assert!(!within_tol(Some(10.0), None, 5.0));
    }

    #[test]
    fn geometry_matches_exact() {
        let a = LayoutElement {
            path: String::new(),
            tag: None,
            x: Some(0.0),
            y: Some(0.0),
            w: Some(100.0),
            h: Some(50.0),
        };
        let b = LayoutElement {
            path: String::new(),
            tag: None,
            x: Some(1.0),
            y: Some(1.0),
            w: Some(103.0),
            h: Some(54.0),
        };
        assert!(geometry_matches(&a, &b));
    }

    #[test]
    fn geometry_matches_outside_tolerance() {
        let a = LayoutElement {
            path: String::new(),
            tag: None,
            x: Some(0.0),
            y: Some(0.0),
            w: Some(100.0),
            h: Some(50.0),
        };
        let b = LayoutElement {
            path: String::new(),
            tag: None,
            x: Some(0.0),
            y: Some(0.0),
            w: Some(106.0),
            h: Some(50.0),
        };
        assert!(!geometry_matches(&a, &b));
    }

    #[test]
    fn head_paths_filtered() {
        assert!(is_head_path("html>head[0]>meta[0]"));
        assert!(is_head_path("html>head"));
        assert!(!is_head_path("html>body[0]>div[0]"));
    }

    #[test]
    fn determine_status_pass() {
        let s = determine_status(5.0, Some(90.0), 10.0, Some(80.0), false, None);
        assert_eq!(s.as_str(), "PASS");
    }

    #[test]
    fn determine_status_fail_pixel() {
        let s = determine_status(15.0, Some(90.0), 10.0, Some(80.0), false, None);
        assert_eq!(s.as_str(), "FAIL_THRESHOLD");
    }

    #[test]
    fn determine_status_fail_element() {
        let s = determine_status(5.0, Some(70.0), 10.0, Some(80.0), false, None);
        assert_eq!(s.as_str(), "FAIL_THRESHOLD");
    }

    #[test]
    fn determine_status_expected_fail() {
        let s = determine_status(15.0, Some(70.0), 10.0, Some(80.0), true, None);
        assert_eq!(s.as_str(), "EXPECTED_FAIL");
    }

    #[test]
    fn determine_status_regression() {
        let s = determine_status(6.0, Some(90.0), 10.0, Some(80.0), false, Some(4.0));
        assert_eq!(s.as_str(), "REGRESSION");
    }

    #[test]
    fn determine_status_within_regression_tolerance() {
        let s = determine_status(4.3, Some(90.0), 10.0, Some(80.0), false, Some(4.0));
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

    #[test]
    fn fuzz_threshold_allows_small_diff() {
        // 5% of 255 = 12.75, threshold is 13
        assert!(12u8 <= FUZZ_THRESHOLD);
        assert!(13u8 <= FUZZ_THRESHOLD);
        assert!(14u8 > FUZZ_THRESHOLD);
    }
}
