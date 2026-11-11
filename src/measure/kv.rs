//! The stderr key=value timing and counter protocol.

#[cfg(test)]
use super::BenchResult;
use super::wall::us_to_ms;
use crate::db::KvPair;
use crate::error::DevError;

/// One stderr line read as a `key=value` field, or `None` when it is not one.
///
/// The line splits at its first `=`; key and value are trimmed. The key must
/// be a field name: an ASCII letter or `_`, then ASCII letters, digits, `_`,
/// `.` or `-`. Anything else - a key with spaces, a colon, a bracket, a
/// leading digit, or nothing at all - makes the line prose, skipped silently.
/// The value is unrestricted (`label=a=b` is `label` -> `a=b`).
///
/// This rejects nonconforming names; it is not a prose detector. A sentence
/// that happens to open with a field name and `=` (`wheel = zoom, drag =
/// pan`) still reads as a field. Every reader of a child's stderr
/// `key=value` lines goes through here, so they share one grammar.
///
/// A field whose key opens with one of brokkr's own namespaces (every
/// `KvClass` prefix: `meta.`, `env.`, `prev.`, `threads.`, `sidecar.`,
/// `provenance.`) is dropped with a warning. Those namespaces mean something
/// the harness observed - `env.*` becomes a row's captured environment and
/// splits compare pairing, `meta.exit_code` is read as the run's answer,
/// `prev.*` describes the preceding run - so a child's diagnostic spelled that
/// way must not land as one. The filter lives here, at the scrape, and not on
/// a row's kv as a whole: brokkr appends its own `meta.*` pairs to a result
/// after the scrape (capture's exit code, the timing source, elivagar's
/// detected mode), and a row-level check cannot tell them apart.
pub(crate) fn kv_line(line: &str) -> Option<(&str, &str)> {
    let (key, value) = field(line)?;
    if crate::db::KvKey::from_stored(key).class() != crate::db::KvClass::Counter {
        // Once per key per process: the scrape runs on every iteration, and
        // N identical warnings say nothing the first did not.
        static WARNED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let first = WARNED.lock().map_or(true, |mut warned| {
            let new = !warned.iter().any(|k| k == key);
            if new {
                warned.push(key.to_owned());
            }
            new
        });
        if first {
            crate::output::console::warn(&format!(
                "dropped `{key}` from the run's output: that namespace is brokkr's own"
            ));
        }
        return None;
    }
    Some((key, value))
}

/// The bare field-name grammar of [`kv_line`], without the namespace filter.
fn field(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    let mut chars = key.chars();
    let first = chars.next()?;
    let named = (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    named.then_some((key, value.trim()))
}

/// Parse stderr `key=value` lines, returning the timing and non-timing pairs.
/// Only lines [`kv_line`] accepts count; other lines are prose.
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
        if let Some((key, value)) = kv_line(line) {
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

/// The timing a self-reporting child printed (the `elapsed_us` capture
/// already parsed from `stderr`), or the error naming its absence with a
/// preview of what it printed instead.
pub(super) fn reported_timing(reported_us: Option<i64>, stderr: &[u8]) -> Result<i64, DevError> {
    reported_us.ok_or_else(|| {
        let text = String::from_utf8_lossy(stderr);
        let preview: String = text
            .chars()
            .take(crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES.stderr_preview_chars)
            .collect();
        DevError::Config(format!(
            "subprocess stderr missing elapsed_ms=NNN. stderr was:\n{preview}"
        ))
    })
}

/// One parse of a self-reporting child's stderr into a result, for the
/// protocol tests; the measurement path parses once in capture and applies
/// [`reported_timing`].
#[cfg(test)]
pub(super) fn parse_kv_stderr(stderr: &[u8]) -> Result<BenchResult, DevError> {
    let (elapsed_us, kv) = parse_kv_lines_us(stderr);
    let elapsed_us = reported_timing(elapsed_us, stderr)?;

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
