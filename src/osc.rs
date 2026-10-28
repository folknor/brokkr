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
//! would be churn. It scans incrementally and retains only the current tag,
//! so large Planet or regional diffs do not need to fit in memory.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufRead, Cursor, Read};
use std::path::Path;

use flate2::read::MultiGzDecoder;

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

/// Parse an OSC file from disk. Gzip is detected by magic bytes (not the
/// filename): pbfhogg's `diff --format osc` always gzips its output regardless
/// of the chosen extension, so a `.osc` file can still be gzip-compressed.
///
/// Decompression goes through `MultiGzDecoder`, not `GzDecoder`, because that
/// OSC is a concatenation of independently-gzipped members (pbfhogg splices
/// the create/modify/delete sections). Plain `GzDecoder` stops silently at the
/// first member boundary and would yield a truncated-but-valid-looking OSC.
pub fn parse_osc_file(path: &Path) -> Result<OscDiff, DevError> {
    let (input, compressed) = open_osc(path)?;
    parse_osc_reader(input).map_err(|error| match error {
        OscReadError::Read(error) if compressed => {
            DevError::Verify(format!("cannot decompress OSC {}: {error}", path.display()))
        }
        OscReadError::Read(error) => {
            DevError::Verify(format!("cannot read OSC {}: {error}", path.display()))
        }
        OscReadError::Utf8(error) => DevError::Verify(format!(
            "OSC {} is not valid UTF-8: {error}",
            path.display()
        )),
        OscReadError::Parse(why) => DevError::Verify(format!(
            "{} is not a complete OSC document: {why}",
            path.display()
        )),
    })
}

/// Open an OSC file as its decompressed byte stream, gzip detected by magic
/// as in [`parse_osc_file`]. The flag says whether it was gzip.
fn open_osc(path: &Path) -> Result<(Box<dyn Read>, bool), DevError> {
    let mut file = File::open(path)
        .map_err(|e| DevError::Verify(format!("cannot open OSC {}: {e}", path.display())))?;
    let mut magic = [0; 2];
    let mut prefix_len = 0;
    while prefix_len < magic.len() {
        let read = file
            .read(&mut magic[prefix_len..])
            .map_err(|e| DevError::Verify(format!("cannot read OSC {}: {e}", path.display())))?;
        if read == 0 {
            break;
        }
        prefix_len += read;
    }
    let input = Cursor::new(magic[..prefix_len].to_vec()).chain(file);
    let compressed = prefix_len == 2 && magic == [0x1f, 0x8b];
    if compressed {
        Ok((Box::new(MultiGzDecoder::new(input)), true))
    } else {
        Ok((Box::new(input), false))
    }
}

/// The offset of the first byte at which two OSC files' decompressed contents
/// differ (where the shorter one ends, if one is a prefix of the other), or
/// `None` when they are identical. The gzip framing is not compared: pbfhogg
/// splices independently-gzipped members, so two runs may frame the same
/// document differently.
pub fn first_content_difference(a: &Path, b: &Path) -> Result<Option<u64>, DevError> {
    let (a_input, _) = open_osc(a)?;
    let (b_input, _) = open_osc(b)?;
    first_stream_difference(a_input, b_input).map_err(|e| {
        DevError::Verify(format!(
            "cannot read OSC {} or {}: {e}",
            a.display(),
            b.display()
        ))
    })
}

/// The offset at which two byte streams first differ, read through fixed
/// buffers; a length difference counts at the shorter stream's end.
pub(crate) fn first_stream_difference(a: impl Read, b: impl Read) -> io::Result<Option<u64>> {
    let mut a = io::BufReader::new(a);
    let mut b = io::BufReader::new(b);
    let mut offset: u64 = 0;
    loop {
        let a_buf = a.fill_buf()?;
        let b_buf = b.fill_buf()?;
        if a_buf.is_empty() || b_buf.is_empty() {
            return Ok((a_buf.len() != b_buf.len()).then_some(offset));
        }
        let n = a_buf.len().min(b_buf.len());
        if let Some(i) = a_buf[..n].iter().zip(&b_buf[..n]).position(|(x, y)| x != y) {
            return Ok(Some(offset + i as u64));
        }
        a.consume(n);
        b.consume(n);
        offset += n as u64;
    }
}

/// Section the parser is currently inside. `None` means "between
/// `<osmChange>` and the first action tag" or "after closing one
/// action tag and before opening the next."
#[derive(Debug, Clone, Copy, Default)]
enum Section {
    #[default]
    None,
    Create,
    Modify,
    Delete,
}

/// Where the parser stands relative to the `<osmChange>` root element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Root {
    /// No element seen yet (only the prolog: `<?xml?>`, comments, DOCTYPE).
    #[default]
    Before,
    Open,
    Closed,
}

/// Read errors and document errors remain distinct so the file entry point
/// can name decompression, UTF-8, and XML failures accurately.
#[derive(Debug)]
enum OscReadError {
    Read(io::Error),
    Utf8(std::str::Utf8Error),
    Parse(String),
}

/// Validate UTF-8 in bounded chunks. The trailing bytes of a partial code
/// point are carried into the next chunk; the OSC scanner itself only retains
/// the current tag (or a small comment marker), never the element bodies.
#[derive(Default)]
struct Utf8Validator {
    trailing: Vec<u8>,
}

impl Utf8Validator {
    fn push(&mut self, bytes: &[u8]) -> Result<(), std::str::Utf8Error> {
        let mut joined = std::mem::take(&mut self.trailing);
        joined.extend_from_slice(bytes);
        match std::str::from_utf8(&joined) {
            Ok(_) => Ok(()),
            Err(error) if error.error_len().is_none() => {
                self.trailing
                    .extend_from_slice(&joined[error.valid_up_to()..]);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn finish(self) -> Result<(), std::str::Utf8Error> {
        std::str::from_utf8(&self.trailing).map(|_| ())
    }
}

fn parse_osc_reader<R: Read>(mut reader: R) -> Result<OscDiff, OscReadError> {
    let mut parser = OscParser::default();
    let mut validator = Utf8Validator::default();
    let mut chunk = [0; 16 * 1024];
    loop {
        let read = reader.read(&mut chunk).map_err(OscReadError::Read)?;
        if read == 0 {
            validator.finish().map_err(OscReadError::Utf8)?;
            break;
        }
        validator.push(&chunk[..read]).map_err(OscReadError::Utf8)?;
        parser.feed(&chunk[..read]).map_err(OscReadError::Parse)?;
    }
    parser.finish().map_err(OscReadError::Parse)
}

/// Parse already-decompressed OSC XML text. Tolerant of whitespace,
/// XML comments, and self-closing elements (`<node id="1" .../>`).
/// Element-body content (tags / refs / members) is silently skipped.
///
/// The document must be an OSC one: its first element must be the
/// `<osmChange>` root and that root must be closed. Without the check, any
/// non-OSC input - an empty file, an `.osm` document, a diff truncated
/// mid-write - parsed as an empty diff, which `verify_merge` reports as
/// "element-identical PASS".
#[cfg(test)]
pub fn parse_osc_text(text: &str) -> Result<OscDiff, String> {
    let mut parser = OscParser::default();
    parser.feed(text.as_bytes())?;
    parser.finish()
}

#[derive(Default)]
struct OscParser {
    out: OscDiff,
    section: Section,
    root: Root,
    scan: ScanState,
}

#[derive(Default)]
enum ScanState {
    #[default]
    Text,
    Probe(Vec<u8>),
    Tag(Vec<u8>),
    Comment(u8),
    Declaration,
}

impl OscParser {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        for &byte in bytes {
            let state = std::mem::take(&mut self.scan);
            self.scan = match state {
                ScanState::Text if byte == b'<' => ScanState::Probe(Vec::new()),
                ScanState::Text => ScanState::Text,
                ScanState::Probe(mut prefix) => {
                    prefix.push(byte);
                    if prefix.as_slice() == b"!--" {
                        ScanState::Comment(0)
                    } else if b"!--".starts_with(prefix.as_slice()) || prefix.as_slice() == b"?" {
                        ScanState::Probe(prefix)
                    } else if prefix.starts_with(b"!") || prefix.starts_with(b"?") {
                        ScanState::Declaration
                    } else if byte == b'>' {
                        self.process_tag(&prefix[..prefix.len() - 1])?;
                        ScanState::Text
                    } else {
                        ScanState::Tag(prefix)
                    }
                }
                ScanState::Tag(tag) if byte == b'>' => {
                    self.process_tag(&tag)?;
                    ScanState::Text
                }
                ScanState::Tag(mut tag) => {
                    tag.push(byte);
                    ScanState::Tag(tag)
                }
                ScanState::Comment(mut trailing_dashes) => {
                    if byte == b'>' && trailing_dashes == 2 {
                        ScanState::Text
                    } else {
                        trailing_dashes = if byte == b'-' {
                            (trailing_dashes + 1).min(2)
                        } else {
                            0
                        };
                        ScanState::Comment(trailing_dashes)
                    }
                }
                ScanState::Declaration if byte == b'>' => ScanState::Text,
                ScanState::Declaration => ScanState::Declaration,
            };
        }
        Ok(())
    }

    fn process_tag(&mut self, tag: &[u8]) -> Result<(), String> {
        let tag = std::str::from_utf8(tag).map_err(|error| error.to_string())?;
        if let Some(name) = tag.strip_prefix('/') {
            let name = name.trim();
            match name {
                "create" | "modify" | "delete" => self.section = Section::None,
                "osmChange" if self.root == Root::Open => self.root = Root::Closed,
                _ => {}
            }
            return Ok(());
        }

        let self_closing = tag.ends_with('/');
        let stripped = tag.trim_end_matches('/').trim();
        let (name, attrs) = split_tag_name(stripped);
        match self.root {
            Root::Before if name == "osmChange" => {
                self.root = if self_closing {
                    Root::Closed
                } else {
                    Root::Open
                };
                return Ok(());
            }
            Root::Before => return Err(format!("root element is <{name}>, not <osmChange>")),
            Root::Closed => return Err(format!("element <{name}> after the closing </osmChange>")),
            Root::Open => {}
        }
        match name {
            "create" => self.section = Section::Create,
            "modify" => self.section = Section::Modify,
            "delete" => self.section = Section::Delete,
            "node" | "way" | "relation" => {
                if let Some(id) = extract_id(attrs) {
                    record(&mut self.out, self.section, name, id);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Result<OscDiff, String> {
        match self.scan {
            ScanState::Text => {}
            ScanState::Comment(_) => return Err("unterminated XML comment".to_owned()),
            ScanState::Declaration => return Err("unterminated XML declaration".to_owned()),
            ScanState::Probe(_) | ScanState::Tag(_) => {
                return Err("incomplete XML tag".to_owned());
            }
        }
        match self.root {
            Root::Closed => Ok(self.out),
            Root::Before => Err("no <osmChange> root element".to_owned()),
            Root::Open => Err("missing closing </osmChange> (truncated?)".to_owned()),
        }
    }
}

fn split_tag_name(tag: &str) -> (&str, &str) {
    let s = tag.trim_start();
    let split = s.find(|c: char| c.is_ascii_whitespace()).unwrap_or(s.len());
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
        let diff = parse_osc_text(SAMPLE).unwrap();
        assert_eq!(
            diff.created_nodes.iter().copied().collect::<Vec<_>>(),
            vec![100]
        );
        assert_eq!(
            diff.created_ways.iter().copied().collect::<Vec<_>>(),
            vec![200]
        );
        assert!(diff.created_relations.is_empty());
        assert_eq!(
            diff.modified_nodes.iter().copied().collect::<Vec<_>>(),
            vec![300]
        );
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![400]
        );
        assert_eq!(
            diff.deleted_ways.iter().copied().collect::<Vec<_>>(),
            vec![500]
        );
        assert_eq!(
            diff.deleted_relations.iter().copied().collect::<Vec<_>>(),
            vec![600]
        );
    }

    #[test]
    fn decodes_multi_member_gzip_osc() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        // pbfhogg's `diff --format osc` splices independently-gzipped
        // fragments; emulate a two-member stream. A plain GzDecoder would stop
        // after the first member - MultiGzDecoder must return both.
        let gz = |s: &str| {
            let mut e = GzEncoder::new(Vec::new(), Compression::default());
            e.write_all(s.as_bytes()).unwrap();
            e.finish().unwrap()
        };
        let mut multi = gz(r#"<osmChange><create><node id="1" version="1"/></create>"#);
        multi.extend(gz(
            r#"<delete><way id="2" version="3"/></delete></osmChange>"#,
        ));
        assert_eq!(&multi[..2], &[0x1f, 0x8b], "gzip magic");

        // Detected as gzip by magic even with a bare `.osc` name.
        let path = crate::test_scratch::scratch_path("osc", "multi-member.osc");
        std::fs::write(&path, multi).unwrap();
        let diff = parse_osc_file(&path).unwrap();
        assert_eq!(
            diff.created_nodes.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            diff.deleted_ways.iter().copied().collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn content_difference_ignores_gzip_framing() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let gz = |s: &str| {
            let mut e = GzEncoder::new(Vec::new(), Compression::default());
            e.write_all(s.as_bytes()).unwrap();
            e.finish().unwrap()
        };
        let doc = r#"<osmChange><create><node id="1" version="1"/></create></osmChange>"#;
        let (head, tail) = doc.split_at(30);
        let mut two_members = gz(head);
        two_members.extend(gz(tail));

        let one = crate::test_scratch::scratch_path("osc", "framing-one.osc.gz");
        let two = crate::test_scratch::scratch_path("osc", "framing-two.osc.gz");
        let plain = crate::test_scratch::scratch_path("osc", "framing-plain.osc");
        std::fs::write(&one, gz(doc)).unwrap();
        std::fs::write(&two, two_members).unwrap();
        std::fs::write(&plain, doc).unwrap();
        assert_eq!(first_content_difference(&one, &two).unwrap(), None);
        assert_eq!(first_content_difference(&one, &plain).unwrap(), None);
    }

    #[test]
    fn content_difference_names_the_first_differing_offset() {
        let at = |a: &str, b: &str| first_stream_difference(a.as_bytes(), b.as_bytes()).unwrap();
        assert_eq!(at("abc", "abc"), None);
        assert_eq!(at("abc", "abd"), Some(2));
        assert_eq!(at("abc", "abcd"), Some(3));
        assert_eq!(at("abcd", "abc"), Some(3));
        assert_eq!(at("", ""), None);
    }

    #[test]
    fn plain_text_osc_passes_through() {
        let diff = parse_osc_reader(Cursor::new(SAMPLE.as_bytes())).unwrap();
        assert!(diff.created_nodes.contains(&100));
        assert!(diff.modified_nodes.contains(&300));
        assert!(diff.deleted_relations.contains(&600));
    }

    #[test]
    fn handles_self_closing_elements() {
        let xml = r#"<osmChange><delete><node id="1" version="1"/><way id="2" version="1"/></delete></osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            diff.deleted_ways.iter().copied().collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn handles_explicit_close_tags() {
        let xml = r#"<osmChange><create>
  <way id="42" version="1"><nd ref="100"/><nd ref="200"/></way>
</create></osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.created_ways.iter().copied().collect::<Vec<_>>(),
            vec![42]
        );
    }

    #[test]
    fn handles_multiple_blocks_of_same_kind() {
        // OSC tools sometimes emit multiple <delete>...</delete> blocks
        // for the same diff (one per object kind, etc).
        let xml = r#"<osmChange>
<delete><node id="1" version="1"/></delete>
<delete><way id="2" version="1"/></delete>
</osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            diff.deleted_ways.iter().copied().collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn ignores_xml_comments() {
        let xml = r#"<osmChange>
<!-- ignored: <node id="999"/> -->
<delete><node id="1" version="1"/></delete>
</osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn ignores_processing_instructions_and_doctype() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE osmChange>
<osmChange><delete><node id="7" version="1"/></delete></osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![7]
        );
    }

    #[test]
    fn handles_negative_ids() {
        // Negative IDs are valid in OSM (placeholder IDs for new
        // objects in JOSM-style edits).
        let xml = r#"<osmChange><create><node id="-12345" version="1" lat="0" lon="0"/></create></osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert!(diff.created_nodes.contains(&-12345));
    }

    #[test]
    fn handles_single_quoted_id() {
        let xml = "<osmChange><delete><node id='1' version='1'/></delete></osmChange>";
        let diff = parse_osc_text(xml).unwrap();
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn ignores_elements_outside_action_blocks() {
        // Some OSC variants put the elements directly under <osmChange>;
        // those aren't real OSC and we ignore them.
        let xml = r#"<osmChange>
<node id="1" version="1"/>
<delete><node id="2" version="1"/></delete>
</osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert!(diff.created_nodes.is_empty());
        assert!(diff.modified_nodes.is_empty());
        assert_eq!(
            diff.deleted_nodes.iter().copied().collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn missing_id_attribute_is_skipped() {
        // Defensive: a malformed element with no id is skipped, not a panic.
        let xml = r#"<osmChange><delete><node version="1"/></delete></osmChange>"#;
        let diff = parse_osc_text(xml).unwrap();
        assert!(diff.is_empty());
    }

    #[test]
    fn empty_osmchange_yields_empty_diff() {
        assert!(
            parse_osc_text("<osmChange></osmChange>")
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_osc_text(r#"<?xml version="1.0"?><osmChange version="0.6"/>"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn non_osc_input_is_refused_not_an_empty_diff() {
        // The bug: each of these parsed as an empty diff, which verify_merge
        // reported as "element-identical PASS".
        let empty = parse_osc_text("").unwrap_err();
        assert!(empty.contains("no <osmChange>"), "got: {empty}");
        assert!(parse_osc_text("   \n").is_err());
        assert!(parse_osc_text("not xml at all").is_err());
        let osm = parse_osc_text(r#"<osm version="0.6"><node id="1"/></osm>"#).unwrap_err();
        assert!(osm.contains("<osm>"), "got: {osm}");
    }

    #[test]
    fn truncated_osc_is_refused() {
        let err = parse_osc_text(r#"<osmChange><delete><node id="1" version="1"/>"#).unwrap_err();
        assert!(err.contains("</osmChange>"), "got: {err}");
        // Cut off inside a tag.
        assert!(parse_osc_text(r#"<osmChange><delete><node id="1" vers"#).is_err());
    }

    #[test]
    fn content_after_root_is_refused() {
        let err = parse_osc_text(
            r#"<osmChange></osmChange><osmChange><delete><node id="1"/></delete></osmChange>"#,
        )
        .unwrap_err();
        assert!(err.contains("after the closing"), "got: {err}");
    }

    #[test]
    fn parse_osc_file_names_the_file_on_refusal() {
        let path = crate::test_scratch::scratch_path("osc", "empty-osc-file");
        std::fs::write(&path, b"").unwrap();
        let err = parse_osc_file(&path).unwrap_err().to_string();
        assert!(err.contains("empty-osc-file"), "got: {err}");
        assert!(err.contains("not a complete OSC document"), "got: {err}");
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
