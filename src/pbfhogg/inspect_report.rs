//! Parser for the human-readable `pbfhogg inspect` report.
//!
//! The parser follows `pbfhogg/src/commands/inspect.rs::print_elements`:
//! counts live in the `Elements:` section, comma separators are formatting,
//! and an extended ID-range section can repeat element labels. `Ordered:` is
//! an independent optional summary field.

/// Element counts printed by the `Elements:` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InspectCounts {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
}

/// Parsed fields from one `pbfhogg inspect` stdout stream.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct InspectReport {
    pub nodes: Option<u64>,
    pub ways: Option<u64>,
    pub relations: Option<u64>,
    pub ordered: Option<bool>,
}

impl InspectReport {
    /// Parse element counts and the optional order summary from one report.
    pub fn parse(stdout: &str) -> Self {
        let mut report = Self::default();
        let mut in_elements = false;

        for line in stdout.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("Elements:") {
                in_elements = true;
                continue;
            }
            if in_elements && trimmed.is_empty() {
                in_elements = false;
            } else if in_elements {
                let Some((label, rest)) = trimmed.split_once(':') else {
                    continue;
                };
                let Some(count) = rest.split_whitespace().next().and_then(parse_count) else {
                    continue;
                };
                match label.trim() {
                    "Nodes" => report.nodes = Some(count),
                    "Ways" => report.ways = Some(count),
                    "Relations" => report.relations = Some(count),
                    _ => {}
                }
            }

            if let Some(rest) = trimmed.strip_prefix("Ordered:") {
                match rest.trim() {
                    "yes" => report.ordered = Some(true),
                    "no" => report.ordered = Some(false),
                    _ => {}
                }
            }
        }

        report
    }
}

fn parse_count(token: &str) -> Option<u64> {
    token
        .chars()
        .filter(|ch| *ch != ',')
        .collect::<String>()
        .parse()
        .ok()
}
