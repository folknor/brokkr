//! Physical and swap memory readings from `/proc/meminfo`.

/// Memory totals in MiB. Missing or unreadable procfs values are zero, which
/// keeps report output useful in restricted environments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MemoryInfo {
    pub total_mb: u64,
    pub available_mb: u64,
    pub swap_total_mb: u64,
    pub swap_free_mb: u64,
}

pub(crate) fn collect() -> MemoryInfo {
    let content = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    MemoryInfo {
        total_mb: parse_meminfo_field(&content, "MemTotal:"),
        available_mb: parse_meminfo_field(&content, "MemAvailable:"),
        swap_total_mb: parse_meminfo_field(&content, "SwapTotal:"),
        swap_free_mb: parse_meminfo_field(&content, "SwapFree:"),
    }
}

pub(crate) fn parse_meminfo_field(content: &str, prefix: &str) -> u64 {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            return parse_kb_to_mb(rest);
        }
    }
    0
}

pub(crate) fn parse_kb_to_mb(rest: &str) -> u64 {
    let kb: u64 = rest
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    kb / 1024
}
