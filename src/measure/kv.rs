//! The stderr key=value timing and counter protocol.

use crate::db::KvPair;

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
/// after the scrape (capture's exit code, the timing source), and a row-level
/// check cannot tell them apart.
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

/// Parse stderr `key=value` lines into counter pairs. Only lines [`kv_line`]
/// accepts count; other lines are prose. `elapsed_ms`/`total_ms` are reserved
/// timing names: brokkr times the child itself, so a child's own timing line
/// is skipped rather than stored as a counter beside the wall.
pub(crate) fn parse_kv_lines(stderr: &[u8]) -> Vec<KvPair> {
    let text = String::from_utf8_lossy(stderr);
    let mut kv = Vec::new();

    for line in text.lines() {
        if let Some((key, value)) = kv_line(line) {
            if key == "elapsed_ms" || key == "total_ms" {
                // Reserved: see the doc comment.
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

    kv
}
