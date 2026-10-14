//! The stderr key=value timing and counter protocol.

use super::BenchResult;
use super::wall::us_to_ms;
use crate::db::KvPair;
use crate::error::DevError;

/// Parse stderr `key=value` lines, returning the timing and non-timing pairs.
/// `elapsed_ms`/`total_ms` are reserved timing names and are not added to the
/// returned kv pairs; the last valid timing line wins.
///
/// `elapsed_ms` is `None` when no `elapsed_ms`/`total_ms` line is found.
pub(crate) fn parse_kv_lines(stderr: &[u8]) -> (Option<i64>, Vec<KvPair>) {
    let (us, kv) = parse_kv_lines_us(stderr);
    (us.map(us_to_ms), kv)
}

/// As [`parse_kv_lines`], but returns the timing in microseconds.
///
/// The timing line may be fractional (`elapsed_ms=6.847`). It used to be
/// parsed as `i64`, so a fractional value failed to parse at all and the run
/// then died on the "missing elapsed_ms" check - a target reporting *more*
/// precision than brokkr asked for was treated as reporting none.
pub(crate) fn parse_kv_lines_us(stderr: &[u8]) -> (Option<i64>, Vec<KvPair>) {
    let text = String::from_utf8_lossy(stderr);
    let mut elapsed_us: Option<i64> = None;
    let mut kv = Vec::new();

    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if key == "elapsed_ms" || key == "total_ms" {
                // Integer first so whole-millisecond values stay exact rather
                // than making a round trip through f64.
                if let Ok(ms) = value.parse::<i64>() {
                    elapsed_us = Some(ms.saturating_mul(1000));
                } else if let Ok(ms) = value.parse::<f64>()
                    && ms.is_finite()
                    && ms >= 0.0
                {
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        elapsed_us = Some((ms * 1000.0).round() as i64);
                    }
                }
            } else if let Ok(n) = value.parse::<i64>() {
                kv.push(KvPair::int(key, n));
            } else if let Ok(f) = value.parse::<f64>() {
                if f.is_finite() {
                    kv.push(KvPair::real(key, f));
                } else {
                    kv.push(KvPair::text(key, value));
                }
            } else {
                kv.push(KvPair::text(key, value));
            }
        }
    }

    (elapsed_us, kv)
}

pub(super) fn parse_kv_stderr(stderr: &[u8]) -> Result<BenchResult, DevError> {
    let (elapsed_us, kv) = parse_kv_lines_us(stderr);

    let elapsed_us = elapsed_us.ok_or_else(|| {
        let text = String::from_utf8_lossy(stderr);
        let preview: String = text
            .chars()
            .take(crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES.stderr_preview_chars)
            .collect();
        DevError::Config(format!(
            "subprocess stderr missing elapsed_ms=NNN. stderr was:\n{preview}"
        ))
    })?;

    Ok(BenchResult {
        elapsed_ms: us_to_ms(elapsed_us),
        // This is the one path that knows sub-millisecond timing: the target
        // reported it directly.
        elapsed_us: Some(elapsed_us),
        kv,
        // One iteration's parse - the enclosing loop owns the list.
        iterations: Vec::new(),
        distribution: None,
        hotpath: None,
    })
}
