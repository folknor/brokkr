//! Shared timing policy and duration spelling for ratatoskr orchestration.

use std::time::Duration;

/// Time allowed for sæhrimnir to publish its readiness sentinel.
pub const READINESS_BUDGET: Duration = Duration::from_secs(10);

/// Grace period before a stopped child is escalated to SIGKILL.
pub const SHUTDOWN_BUDGET: Duration = Duration::from_millis(1500);

/// Default per-script wall-clock ceiling when frontmatter omits one.
pub const DEFAULT_CEILING: Duration = Duration::from_secs(60);

/// Poll cadence used by sæhrimnir readiness and signal waits.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Poll cadence while waiting for a child to exit after SIGTERM.
pub(crate) const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Parse the positive integer duration grammar used by script frontmatter.
/// Bare numbers mean seconds; supported suffixes are `ms`, `s`, `m`, and `h`.
pub(crate) fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (num_part, unit) = split_unit(value);
    let n: u64 = num_part.parse().ok()?;
    if n == 0 {
        return None;
    }
    match unit {
        "" | "s" => Some(Duration::from_secs(n)),
        "ms" => Some(Duration::from_millis(n)),
        "m" => Some(Duration::from_secs(n.checked_mul(60)?)),
        "h" => Some(Duration::from_secs(n.checked_mul(3600)?)),
        _ => None,
    }
}

fn split_unit(value: &str) -> (&str, &str) {
    let split = value
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(value.len());
    (&value[..split], &value[split..])
}

/// Format a duration in the same grammar accepted by [`parse_duration`].
pub(crate) fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if !duration.subsec_nanos().is_multiple_of(1_000_000) {
        return format!("{millis}ms");
    }
    let secs = duration.as_secs();
    if secs == 0 {
        return format!("{millis}ms");
    }
    if duration.subsec_millis() != 0 {
        return format!("{millis}ms");
    }
    if secs.is_multiple_of(3600) {
        return format!("{}h", secs / 3600);
    }
    if secs.is_multiple_of(60) {
        return format!("{}m", secs / 60);
    }
    format!("{secs}s")
}

/// Format elapsed time in the integer-millisecond unit used by ratatoskr
/// run summaries. Stored durations may retain higher precision separately.
pub(crate) fn format_elapsed(duration: Duration) -> String {
    format_elapsed_ms(duration.as_millis())
}

/// Format an already-truncated elapsed millisecond count.
pub(crate) fn format_elapsed_ms(milliseconds: impl std::fmt::Display) -> String {
    format!("{milliseconds}ms")
}

#[cfg(test)]
mod tests {
    use super::{format_duration, parse_duration};
    use std::time::Duration;

    #[test]
    fn duration_spelling_round_trips_at_millisecond_precision() {
        for duration in [
            Duration::from_millis(1),
            Duration::from_millis(500),
            Duration::from_millis(1_500),
            Duration::from_secs(1),
            Duration::from_secs(90),
            Duration::from_secs(120),
            Duration::from_secs(3_600),
            Duration::from_secs(7_200),
        ] {
            let spelling = format_duration(duration);
            assert_eq!(parse_duration(&spelling), Some(duration), "{spelling}");
        }
    }
}
