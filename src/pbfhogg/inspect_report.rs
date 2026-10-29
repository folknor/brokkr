//! What verify reads from `pbfhogg inspect --json`.
//!
//! Every reader runs inspect with its stdout written to a file beside the
//! inspected output (`<stem>.inspect.json`, or `<stem>.inspect-extended.json`
//! under `--extended`) and reads it back through a buffered streaming
//! deserializer that retains only the fields below and skips every other
//! value as it streams past, without collecting its contents. The reader is
//! keyed on fields, not on `schema_version`: a report missing a field read
//! here is refused, but a read field whose meaning changes under a new
//! schema is not detected. The file stays in the check's directory as the
//! full report.

use std::path::{Path, PathBuf};

use super::verify::{PbfArgv, SORT_TYPE_THEN_ID, VerifyHarness};
use crate::error::DevError;

/// Element counts of one PBF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InspectCounts {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
}

/// The bounded part of one `inspect --json` report.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub(super) struct InspectSummary {
    header: Header,
    pub(super) elements: Elements,
    ordering: Ordering,
    /// Present only under `--extended`.
    #[serde(default)]
    data: Option<ExtendedData>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct Header {
    required_features: Vec<String>,
    optional_features: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub(super) struct Elements {
    nodes: u64,
    ways: u64,
    relations: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
struct Ordering {
    standard: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct ExtendedData {
    /// `[min_lon, min_lat, max_lon, max_lat]`, null for a file without nodes.
    bbox: Option<[f64; 4]>,
    timestamp: Option<Timestamps>,
    objects_ordered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct Timestamps {
    first: String,
    last: String,
}

impl InspectSummary {
    pub(super) fn counts(&self) -> InspectCounts {
        InspectCounts {
            nodes: self.elements.nodes,
            ways: self.elements.ways,
            relations: self.elements.relations,
        }
    }

    /// Whether the header declares `Sort.Type_then_ID`, in either feature
    /// list (the human report's `Features:` line merges both).
    pub(super) fn declares_sorted(&self) -> bool {
        self.header
            .required_features
            .iter()
            .chain(&self.header.optional_features)
            .any(|f| f == SORT_TYPE_THEN_ID)
    }

    /// The computed element order (standard ordering and per-kind ID
    /// monotonicity), `None` unless the report is extended.
    pub(super) fn objects_ordered(&self) -> Option<bool> {
        self.data.as_ref().map(|d| d.objects_ordered)
    }

    /// The summary as operator lines, each prefixed with `label`.
    pub(super) fn lines(&self, label: &str) -> Vec<String> {
        let e = &self.elements;
        let mut features: Vec<&str> = self
            .header
            .required_features
            .iter()
            .chain(&self.header.optional_features)
            .map(String::as_str)
            .collect();
        features.dedup();
        let mut lines = vec![
            format!(
                "{label}: elements: {} nodes, {} ways, {} relations",
                e.nodes, e.ways, e.relations
            ),
            format!("{label}: features: {}", features.join(", ")),
            format!(
                "{label}: ordering: {}",
                if self.ordering.standard {
                    "standard"
                } else {
                    "NON-STANDARD"
                }
            ),
        ];
        if let Some(data) = &self.data {
            lines.push(format!(
                "{label}: ordered: {}",
                if data.objects_ordered { "yes" } else { "no" }
            ));
            if let Some([min_lon, min_lat, max_lon, max_lat]) = data.bbox {
                lines.push(format!(
                    "{label}: data bbox: {min_lon},{min_lat},{max_lon},{max_lat}"
                ));
            }
            if let Some(t) = &data.timestamp {
                lines.push(format!("{label}: timestamps: {} .. {}", t.first, t.last));
            }
        }
        lines
    }
}

fn parse_summary(reader: impl std::io::Read) -> serde_json::Result<InspectSummary> {
    serde_json::from_reader(reader)
}

/// Where the report of `pbf` is written: beside it, in its check's directory.
pub(super) fn report_path(pbf: &Path, extended: bool) -> PathBuf {
    pbf.with_extension(if extended {
        "inspect-extended.json"
    } else {
        "inspect.json"
    })
}

/// Run `pbfhogg inspect --json` (with `--extended` when asked) on `pbf`, its
/// report written to [`report_path`], and read the bounded summary back.
/// A non-zero exit is an error naming the program; an unreadable report is
/// an error naming the file. An extended report without
/// `data.objects_ordered` is refused, never read as unordered.
pub(super) fn inspect(
    harness: &VerifyHarness,
    pbf: &Path,
    extended: bool,
) -> Result<InspectSummary, DevError> {
    let report = report_path(pbf, extended);
    let argv = PbfArgv::new("inspect")
        .arg("--json")
        .arg_if(extended, "--extended")
        .input(pbf)?;
    let program = if extended {
        "pbfhogg inspect --json --extended"
    } else {
        "pbfhogg inspect --json"
    };
    let captured = harness.run_pbfhogg_to(argv, &report)?;
    harness.check_exit(&captured, program)?;
    read_report(&report, extended)
}

fn read_report(report: &Path, extended: bool) -> Result<InspectSummary, DevError> {
    let file = std::fs::File::open(report)
        .map_err(|e| DevError::Verify(format!("cannot open {}: {e}", report.display())))?;
    let summary = parse_summary(std::io::BufReader::new(file)).map_err(|e| {
        DevError::Verify(format!(
            "cannot read the inspect report {}: {e}",
            report.display()
        ))
    })?;
    if extended && summary.data.is_none() {
        return Err(DevError::Verify(format!(
            "the inspect report {} has no data.objects_ordered although --extended was \
             passed; this pbfhogg does not compute element order",
            report.display()
        )));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Mirrors pbfhogg's schema_version 2 shape: blocks 0 and 2 hold nodes,
    /// 1 and 3 ways, so four ordering segments and a non-standard order.
    const PLAIN: &str = r#"{"schema_version":2,"file":"x.osm.pbf","file_size":10,
        "header":{"writing_program":"pbfhogg","required_features":["OsmSchema-V0.6","DenseNodes"],
          "optional_features":["Sort.Type_then_ID"],"bbox":null,
          "replication":{"sequence":null,"timestamp":null,"url":null}},
        "indexed":true,"blocks":{"total":4,
          "nodes":{"count":2,"compressed_bytes":4,"elements":5},
          "ways":{"count":2,"compressed_bytes":3,"elements":2},
          "relations":{"count":0,"compressed_bytes":0,"elements":0},
          "mixed":{"count":0,"compressed_bytes":0,"elements":0}},
        "elements":{"nodes":5,"tagged_nodes":1,"ways":2,"relations":0,"total":7},
        "ordering":{"standard":false,"segment_count":4,"segments_head":[
          {"kind":"nodes","first_block":0,"last_block":0},
          {"kind":"ways","first_block":1,"last_block":1},
          {"kind":"nodes","first_block":2,"last_block":2},
          {"kind":"ways","first_block":3,"last_block":3}]},
        "id_ranges":null,"anomalies_only":false,"blocks_detail":null,"locations":null}"#;

    #[test]
    fn plain_report_reads_counts_flag_and_ordering_and_skips_the_rest() {
        let s = parse_summary(PLAIN.as_bytes()).unwrap();
        assert_eq!(
            s.counts(),
            InspectCounts {
                nodes: 5,
                ways: 2,
                relations: 0
            }
        );
        assert!(s.declares_sorted());
        assert!(!s.ordering.standard);
        assert_eq!(s.objects_ordered(), None);
        assert!(
            s.lines("p")
                .iter()
                .any(|l| l == "p: ordering: NON-STANDARD")
        );
    }

    #[test]
    fn sorted_flag_is_an_exact_feature_not_a_substring() {
        // The old reader tested the whole human report for the substring, so
        // a file or program name carrying it read as sorted.
        let text = PLAIN
            .replace(
                r#""optional_features":["Sort.Type_then_ID"]"#,
                r#""optional_features":[]"#,
            )
            .replace(
                r#""file":"x.osm.pbf""#,
                r#""file":"Sort.Type_then_ID.osm.pbf""#,
            );
        assert!(!parse_summary(text.as_bytes()).unwrap().declares_sorted());
        let required = PLAIN
            .replace(
                r#""optional_features":["Sort.Type_then_ID"]"#,
                r#""optional_features":[]"#,
            )
            .replace(r#""DenseNodes"]"#, r#""DenseNodes","Sort.Type_then_ID"]"#);
        assert!(
            parse_summary(required.as_bytes())
                .unwrap()
                .declares_sorted()
        );
    }

    #[test]
    fn extended_report_carries_the_computed_order() {
        let text = PLAIN.replace(
            r#""locations":null}"#,
            r#""locations":null,"data":{"bbox":[8.0,54.5,15.2,57.8],
              "timestamp":{"first":"2010-01-01T00:00:00Z","last":"2026-10-01T00:00:00Z"},
              "objects_ordered":false},"metadata":{"all_objects":{}}}"#,
        );
        let s = parse_summary(text.as_bytes()).unwrap();
        assert_eq!(s.objects_ordered(), Some(false));
        let lines = s.lines("p");
        assert!(lines.contains(&"p: ordered: no".to_owned()), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("p: data bbox: 8,54.5,")));
    }

    #[test]
    fn missing_counts_are_refused_not_zero() {
        let text = PLAIN.replace(r#""relations":0,"#, "");
        assert!(parse_summary(text.as_bytes()).is_err());
    }

    #[test]
    fn an_extended_report_without_order_is_refused_with_its_path() {
        let dir = crate::test_scratch::scratch("pbfhogg-verify", "inspect-report");
        let path = dir.join("out.inspect-extended.json");
        std::fs::write(&path, PLAIN).unwrap();
        let err = read_report(&path, true).unwrap_err().to_string();
        assert!(err.contains("out.inspect-extended.json"), "{err}");
        assert!(read_report(&path, false).is_ok());
        let bad = dir.join("bad.inspect.json");
        std::fs::write(&bad, "{").unwrap();
        let err = read_report(&bad, false).unwrap_err().to_string();
        assert!(err.contains("bad.inspect.json"), "{err}");
    }

    #[test]
    fn report_sits_beside_the_output() {
        let p = Path::new("/v/merge/pbfhogg.osm.pbf");
        assert_eq!(
            report_path(p, false),
            Path::new("/v/merge/pbfhogg.osm.inspect.json")
        );
        assert_eq!(
            report_path(p, true),
            Path::new("/v/merge/pbfhogg.osm.inspect-extended.json")
        );
    }
}
