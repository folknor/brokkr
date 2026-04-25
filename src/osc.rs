//! Minimal OSC (`.osc` / `.osc.gz`) reader for verify-side delta analysis.
//!
//! brokkr's verify pipeline needs to know which element IDs an OSC file
//! creates / modifies / deletes - specifically so `verify_merge` can
//! tell legitimate cross-tool semantic differences (osmium's
//! version-based deletes vs pbfhogg's unconditional deletes) apart
//! from real bugs (request 4).
//!
//! The parser is deliberately narrow: it understands `<osmChange>`
//! blocks, the `<create>` / `<modify>` / `<delete>` containers, and
//! `<node id="N">` / `<way id="N">` / `<relation id="N">` element
//! starts within them. It does **not** parse element bodies (tags,
//! refs, members, coordinates, metadata) - those aren't needed for
//! the carve-out and pulling in a full XML stack just to discard them
//! would be churn. OSC files we see are tool-emitted and regular
//! enough that whitespace-tolerant tag-start scanning is sufficient;
//! we never parse user-authored XML.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;

use crate::error::DevError;

/// Per-element-type sets of IDs partitioned by OSC operation.
///
/// The IDs are stored sorted (`BTreeSet`) so iteration is
/// deterministic - useful when the carve-out reports unexplained
/// failures, and when this struct is used to pretty-print summaries.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OscDiff {
    pub created_nodes: BTreeSet<i64>,
    pub created_ways: BTreeSet<i64>,
    pub created_relations: BTreeSet<i64>,

    pub modified_nodes: BTreeSet<i64>,
    pub modified_ways: BTreeSet<i64>,
    pub modified_relations: BTreeSet<i64>,

    pub deleted_nodes: BTreeSet<i64>,
    pub deleted_ways: BTreeSet<i64>,
    pub deleted_relations: BTreeSet<i64>,
}

impl OscDiff {
    /// Total number of element IDs across every section. Useful for
    /// "no deltas at all" check in callers.
    pub fn is_empty(&self) -> bool {
        self.created_nodes.is_empty()
            && self.created_ways.is_empty()
            && self.created_relations.is_empty()
            && self.modified_nodes.is_empty()
            && self.modified_ways.is_empty()
            && self.modified_relations.is_empty()
            && self.deleted_nodes.is_empty()
            && self.deleted_ways.is_empty()
            && self.deleted_relations.is_empty()
    }
}

/// Parse an OSC file from disk. `.gz` extension triggers transparent
/// gzip decompression; everything else is treated as plain XML.
pub fn parse_osc_file(path: &Path) -> Result<OscDiff, DevError> {
    let mut file = File::open(path).map_err(|e| {
        DevError::Verify(format!("cannot open OSC {}: {e}", path.display()))
    })?;
    let mut buf = String::new();
    let is_gz = path.extension().is_some_and(|e| e == "gz");
    if is_gz {
        let mut decoder = GzDecoder::new(file);
        decoder.read_to_string(&mut buf).map_err(|e| {
            DevError::Verify(format!("cannot decompress OSC {}: {e}", path.display()))
        })?;
    } else {
        file.read_to_string(&mut buf).map_err(|e| {
            DevError::Verify(format!("cannot read OSC {}: {e}", path.display()))
        })?;
    }
    Ok(parse_osc_text(&buf))
}

/// Section the parser is currently inside. `None` means "between
/// `<osmChange>` and the first action tag" or "after closing one
/// action tag and before opening the next."
#[derive(Debug, Clone, Copy)]
enum Section {
    None,
    Create,
    Modify,
    Delete,
}

/// Parse already-decompressed OSC XML text. Tolerant of whitespace,
/// XML comments, and self-closing elements (`<node id="1" .../>`).
/// Element-body content (tags / refs / members) is silently skipped.
#[allow(clippy::cognitive_complexity)] // single state machine
pub fn parse_osc_text(text: &str) -> OscDiff {
    let mut out = OscDiff::default();
    let mut section = Section::None;
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Skip until next `<`. Everything before is element bodies or
        // whitespace we don't care about.
        let Some(rel) = bytes[i..].iter().position(|&b| b == b'<') else {
            break;
        };
        i += rel;

        // Skip XML comments `<!-- ... -->` and processing
        // instructions `<? ... ?>` / DOCTYPE `<! ... >`. These never
        // contain action-tag starts in well-formed OSC.
        if bytes[i..].starts_with(b"<!--") {
            if let Some(end) = find_subsequence(&bytes[i..], b"-->") {
                i += end + 3;
                continue;
            }
            break;
        }
        if bytes[i..].starts_with(b"<?") || bytes[i..].starts_with(b"<!") {
            if let Some(end) = bytes[i..].iter().position(|&b| b == b'>') {
                i += end + 1;
                continue;
            }
            break;
        }

        // We're at a `<...>` tag. Find its end.
        let Some(close_rel) = bytes[i..].iter().position(|&b| b == b'>') else {
            break;
        };
        let tag_end = i + close_rel; // exclusive of '>'
        let tag = &text[i + 1..tag_end];
        i = tag_end + 1;

        // Distinguish closing tags (`</foo>`) from opening / self-closing.
        if let Some(name) = tag.strip_prefix('/') {
            let name = name.trim();
            match name {
                "create" | "modify" | "delete" => section = Section::None,
                _ => {}
            }
            continue;
        }

        // Strip self-closing trailing slash for matching.
        let stripped = tag.trim_end_matches('/').trim();
        let (name, attrs) = split_tag_name(stripped);

        match name {
            "create" => section = Section::Create,
            "modify" => section = Section::Modify,
            "delete" => section = Section::Delete,
            "node" | "way" | "relation" => {
                if let Some(id) = extract_id(attrs) {
                    record(&mut out, section, name, id);
                }
            }
            _ => {}
        }
    }

    out
}

fn split_tag_name(tag: &str) -> (&str, &str) {
    let s = tag.trim_start();
    let split = s
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(s.len());
    (&s[..split], s[split..].trim_start())
}

/// Extract the value of the `id` attribute from a tag's attribute
/// string. Returns `None` if absent or unparseable. Tolerant of
/// single- and double-quoted values; doesn't validate other attribute
/// shapes.
fn extract_id(attrs: &str) -> Option<i64> {
    // Walk through attributes looking for `id=`. Simple linear scan -
    // attributes can appear in any order and we don't care about the
    // others.
    let bytes = attrs.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        // Read attribute name.
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name = &attrs[name_start..i];
        // Skip whitespace before `=`.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            return None;
        }
        i += 1; // skip '='
        // Skip whitespace after `=`.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let quote = bytes[i];
        if quote != b'"' && quote != b'\'' {
            return None;
        }
        i += 1;
        let val_start = i;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let val = &attrs[val_start..i];
        i += 1; // skip closing quote
        if name == "id" {
            return val.parse().ok();
        }
    }
    None
}

fn record(out: &mut OscDiff, section: Section, kind: &str, id: i64) {
    let target = match (section, kind) {
        (Section::Create, "node") => &mut out.created_nodes,
        (Section::Create, "way") => &mut out.created_ways,
        (Section::Create, "relation") => &mut out.created_relations,
        (Section::Modify, "node") => &mut out.modified_nodes,
        (Section::Modify, "way") => &mut out.modified_ways,
        (Section::Modify, "relation") => &mut out.modified_relations,
        (Section::Delete, "node") => &mut out.deleted_nodes,
        (Section::Delete, "way") => &mut out.deleted_ways,
        (Section::Delete, "relation") => &mut out.deleted_relations,
        _ => return,
    };
    target.insert(id);
}

/// Naive `memmem` substitute. OSC files we parse top out at
/// double-digit megabytes (the Denmark daily diff) so a linear scan
/// is fine; pulling in the `memchr` crate just to find `-->` would
/// be silly.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6" generator="test">
  <create>
    <node id="100" version="1" lat="55.0" lon="12.0"/>
    <way id="200" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
    </way>
  </create>
  <modify>
    <node id="300" version="2" lat="55.1" lon="12.1"/>
  </modify>
  <delete>
    <node id="400" version="3"/>
    <way id="500" version="2"/>
    <relation id="600" version="1"/>
  </delete>
</osmChange>
"#;

    #[test]
    fn parses_basic_sections() {
        let diff = parse_osc_text(SAMPLE);
        assert_eq!(diff.created_nodes.iter().copied().collect::<Vec<_>>(), vec![100]);
        assert_eq!(diff.created_ways.iter().copied().collect::<Vec<_>>(), vec![200]);
        assert!(diff.created_relations.is_empty());
        assert_eq!(diff.modified_nodes.iter().copied().collect::<Vec<_>>(), vec![300]);
        assert_eq!(diff.deleted_nodes.iter().copied().collect::<Vec<_>>(), vec![400]);
        assert_eq!(diff.deleted_ways.iter().copied().collect::<Vec<_>>(), vec![500]);
        assert_eq!(diff.deleted_relations.iter().copied().collect::<Vec<_>>(), vec![600]);
    }

    #[test]
    fn handles_self_closing_elements() {
        let xml = r#"<osmChange><delete><node id="1" version="1"/><way id="2" version="1"/></delete></osmChange>"#;
        let diff = parse_osc_text(xml);
        assert_eq!(diff.deleted_nodes.iter().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(diff.deleted_ways.iter().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn handles_explicit_close_tags() {
        let xml = r#"<osmChange><create>
  <way id="42" version="1"><nd ref="100"/><nd ref="200"/></way>
</create></osmChange>"#;
        let diff = parse_osc_text(xml);
        assert_eq!(diff.created_ways.iter().copied().collect::<Vec<_>>(), vec![42]);
    }

    #[test]
    fn handles_multiple_blocks_of_same_kind() {
        // OSC tools sometimes emit multiple <delete>...</delete> blocks
        // for the same diff (one per object kind, etc).
        let xml = r#"<osmChange>
<delete><node id="1" version="1"/></delete>
<delete><way id="2" version="1"/></delete>
</osmChange>"#;
        let diff = parse_osc_text(xml);
        assert_eq!(diff.deleted_nodes.iter().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(diff.deleted_ways.iter().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn ignores_xml_comments() {
        let xml = r#"<osmChange>
<!-- ignored: <node id="999"/> -->
<delete><node id="1" version="1"/></delete>
</osmChange>"#;
        let diff = parse_osc_text(xml);
        assert_eq!(diff.deleted_nodes.iter().copied().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn ignores_processing_instructions_and_doctype() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE osmChange>
<osmChange><delete><node id="7" version="1"/></delete></osmChange>"#;
        let diff = parse_osc_text(xml);
        assert_eq!(diff.deleted_nodes.iter().copied().collect::<Vec<_>>(), vec![7]);
    }

    #[test]
    fn handles_negative_ids() {
        // Negative IDs are valid in OSM (placeholder IDs for new
        // objects in JOSM-style edits).
        let xml = r#"<osmChange><create><node id="-12345" version="1" lat="0" lon="0"/></create></osmChange>"#;
        let diff = parse_osc_text(xml);
        assert!(diff.created_nodes.contains(&-12345));
    }

    #[test]
    fn handles_single_quoted_id() {
        let xml = "<osmChange><delete><node id='1' version='1'/></delete></osmChange>";
        let diff = parse_osc_text(xml);
        assert_eq!(diff.deleted_nodes.iter().copied().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn ignores_elements_outside_action_blocks() {
        // Some OSC variants put the elements directly under <osmChange>;
        // those aren't real OSC and we ignore them.
        let xml = r#"<osmChange>
<node id="1" version="1"/>
<delete><node id="2" version="1"/></delete>
</osmChange>"#;
        let diff = parse_osc_text(xml);
        assert!(diff.created_nodes.is_empty());
        assert!(diff.modified_nodes.is_empty());
        assert_eq!(diff.deleted_nodes.iter().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn missing_id_attribute_is_skipped() {
        // Defensive: a malformed element with no id is skipped, not a panic.
        let xml = r#"<osmChange><delete><node version="1"/></delete></osmChange>"#;
        let diff = parse_osc_text(xml);
        assert!(diff.is_empty());
    }

    #[test]
    fn empty_input_yields_empty_diff() {
        assert!(parse_osc_text("").is_empty());
        assert!(parse_osc_text("<osmChange></osmChange>").is_empty());
    }

    #[test]
    fn extract_id_handles_attrs_in_any_order() {
        assert_eq!(extract_id(r#"version="1" id="42""#), Some(42));
        assert_eq!(extract_id(r#"id="42" version="1""#), Some(42));
        assert_eq!(extract_id(r#"  id =  "42"  "#), Some(42));
    }

    #[test]
    fn split_tag_name_separates_name_and_attrs() {
        assert_eq!(split_tag_name(r#"node id="1""#), ("node", r#"id="1""#));
        assert_eq!(split_tag_name("create"), ("create", ""));
    }
}
