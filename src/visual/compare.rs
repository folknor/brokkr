//! Pixel comparison and artifact identity shared by visual subjects.

use std::path::Path;

use crate::error::DevError;

#[allow(dead_code)]
pub(crate) struct PixelDiffResult {
    pub(crate) diff_pct: f64,
    pub(crate) total_pixels: u64,
    pub(crate) diff_pixels: u64,
}

/// IDs become one filesystem component under the project's artifact root.
pub(crate) fn validate_artifact_id(id: &str, kind: &str) -> Result<(), DevError> {
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.chars().any(|ch| matches!(ch, '/' | '\\' | ':'))
    {
        return Err(DevError::Config(format!(
            "{kind} ID {id:?} must be a single path-safe component"
        )));
    }
    Ok(())
}

const FUZZ_THRESHOLD: u8 = 13;

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

    Ok(DecodedPng {
        width,
        height,
        rgba: to_rgba(&buf, color_type, bit_depth)?,
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
                rgba.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
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
    let mut out = vec![255u8; tw * th * 4];
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
        let dr = pipe_rgba[base].abs_diff(ref_rgba[base]);
        let dg = pipe_rgba[base + 1].abs_diff(ref_rgba[base + 1]);
        let db = pipe_rgba[base + 2].abs_diff(ref_rgba[base + 2]);

        if dr > FUZZ_THRESHOLD || dg > FUZZ_THRESHOLD || db > FUZZ_THRESHOLD {
            diff_pixels += 1;
            diff_rgba[base..base + 4].copy_from_slice(&[255, 0, 0, 255]);
        } else {
            diff_rgba[base..base + 4].copy_from_slice(&[
                ref_rgba[base] / 3,
                ref_rgba[base + 1] / 3,
                ref_rgba[base + 2] / 3,
                255,
            ]);
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

#[cfg(test)]
mod tests {
    use super::*;

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
