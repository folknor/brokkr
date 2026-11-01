//! The result blocks pbfhogg's in-tree comparison baselines print on stderr:
//! the osmpbf crate (`bench/osmpbf-baseline`) and the planetiler Java bench
//! (`bench/planetiler-baseline`). One process reports several modes, each as
//! a block:
//!
//! ```text
//! ---
//! tool=osmpbf
//! mode=sequential
//! elapsed_ms=1234
//! nodes=...
//! ```
//!
//! A block opens at a line that is exactly `---` (surrounding whitespace
//! aside); text before the first one is the program's header prose and is
//! not read. Inside a block a line with `=` is a `key=value` field and any
//! other line is ignored. The stream is unframed stderr, so that tolerance is
//! partial: a diagnostic without `=` passes, but one with `=` is read as a
//! field (osmpbf stores its fields as KV) and two with the same left-hand
//! side are a repeated key. What would make a row wrong is refused: a block with no fields, a
//! missing or empty `mode`, a missing, non-integer or negative `elapsed_ms`,
//! a key repeated within a block (which value is the result?) and a mode
//! repeated across blocks (two rows under one command name). The whole
//! output is parsed before any row is recorded, so a malformed later block
//! never leaves the earlier ones stored alone.

use crate::error::DevError;

pub(crate) struct ResultBlock {
    pub mode: String,
    pub elapsed_ms: i64,
    /// Every other field, in the order printed.
    pub fields: Vec<(String, String)>,
}

impl ResultBlock {
    /// A field that must be present as a nonnegative integer.
    pub(crate) fn required_count(&self, tool: &str, key: &str) -> Result<i64, DevError> {
        let value = self
            .fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .ok_or_else(|| {
                DevError::Build(format!(
                    "{tool} benchmark result for mode {} is missing {key}",
                    self.mode
                ))
            })?;
        nonnegative(tool, key, value)
    }
}

pub(crate) fn parse(tool: &str, stderr: &str) -> Result<Vec<ResultBlock>, DevError> {
    let mut raw: Vec<Vec<(String, String)>> = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        if line == "---" {
            raw.push(Vec::new());
            continue;
        }
        // Before the first separator there is no block: header prose.
        let Some(block) = raw.last_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if block.iter().any(|(k, _)| k == key) {
            return Err(DevError::Build(format!(
                "{tool} benchmark printed {key} twice in one result block"
            )));
        }
        block.push((key.to_owned(), value.to_owned()));
    }

    let mut blocks: Vec<ResultBlock> = Vec::with_capacity(raw.len());
    for fields in raw {
        if fields.is_empty() {
            return Err(DevError::Build(format!(
                "{tool} benchmark printed a result separator with no result after it"
            )));
        }
        let take = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        let mode = take("mode")
            .filter(|mode| !mode.is_empty())
            .ok_or_else(|| DevError::Build(format!("{tool} benchmark result block has no mode")))?;
        let elapsed = take("elapsed_ms").ok_or_else(|| {
            DevError::Build(format!(
                "{tool} benchmark result for mode {mode} is missing elapsed_ms"
            ))
        })?;
        let elapsed_ms = nonnegative(tool, "elapsed_ms", &elapsed)?;
        if blocks.iter().any(|block| block.mode == mode) {
            return Err(DevError::Build(format!(
                "{tool} benchmark reported mode {mode} twice"
            )));
        }
        let fields = fields
            .into_iter()
            .filter(|(k, _)| k != "mode" && k != "elapsed_ms")
            .collect();
        blocks.push(ResultBlock {
            mode,
            elapsed_ms,
            fields,
        });
    }
    Ok(blocks)
}

fn nonnegative(tool: &str, key: &str, value: &str) -> Result<i64, DevError> {
    let parsed = value
        .parse::<i64>()
        .map_err(|_| DevError::Build(format!("{tool} benchmark emitted invalid {key}: {value}")))?;
    if parsed < 0 {
        return Err(DevError::Build(format!(
            "{tool} benchmark emitted negative {key}: {parsed}"
        )));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::parse;

    fn err(stderr: &str) -> String {
        match parse("t", stderr) {
            Ok(_) => String::from("parsed"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn header_prose_is_skipped_and_blocks_keep_their_fields() {
        let out = "=== t ===\nsize: 3 MB\nk=v before\n---\ntool=t\nmode=a\nelapsed_ms=5\nnodes=7\n\
                   a library warning\n---\nmode=b\nelapsed_ms=0\n";
        let blocks = parse("t", out).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].mode, "a");
        assert_eq!(blocks[0].elapsed_ms, 5);
        assert_eq!(
            blocks[0].fields,
            vec![("tool".into(), "t".into()), ("nodes".into(), "7".into())]
        );
        assert_eq!(blocks[0].required_count("t", "nodes").unwrap(), 7);
        assert!(blocks[1].required_count("t", "nodes").is_err());
    }

    #[test]
    fn a_separator_is_a_whole_line_only() {
        let blocks = parse("t", "---\nmode=a---b\nelapsed_ms=1\n").unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].mode, "a---b");
    }

    #[test]
    fn malformed_results_are_refused() {
        assert!(err("---\nmode=a\nelapsed_ms=1\nmode=b\n").contains("twice in one"));
        assert!(
            err("---\nmode=a\nelapsed_ms=1\n---\nmode=a\nelapsed_ms=2\n").contains("mode a twice")
        );
        assert!(err("---\nelapsed_ms=1\n").contains("no mode"));
        assert!(err("---\nmode=\nelapsed_ms=1\n").contains("no mode"));
        assert!(err("---\nmode=a\n").contains("missing elapsed_ms"));
        assert!(err("---\nmode=a\nelapsed_ms=-1\n").contains("negative"));
        assert!(err("---\nmode=a\nelapsed_ms=x\n").contains("invalid"));
        assert!(err("---\nmode=a\nelapsed_ms=1\n---\n").contains("no result after"));
    }
}
