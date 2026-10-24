//! Which physical rustflags elements a `strip_rustflags` entry removes.
//!
//! The contract is physical token-sequence deletion, not rustc argv
//! semantics. A rustflags value is a sequence of elements (an array's strings,
//! or a string's whitespace-split tokens). Elements group into *units* left to
//! right: an element exactly `-C` or `-Z` with a following element forms one
//! unit with it, rendered `-C<next>` / `-Z<next>`; every other element is a
//! unit of its own. A spec groups the same way, so `-Cx` and `-C x` name the
//! same unit, and a payload alone (`link-arg=...`) never matches the second
//! half of a `-C` pair.
//!
//! Every match of every spec is found against the original unit sequence
//! before anything is removed, so removal never creates a new match. Two
//! matches that overlap without being the same run make the value ambiguous
//! and refuse. A value with no match is left byte-for-byte as it was.
//!
//! The usability checks refuse a match brokkr cannot place with confidence
//! inside its own value: a response file (`@path`) or `--` anywhere before it,
//! or an immediately preceding option that may take the match as its operand.
//! They are checks on the physical value, not a proof that the removed tokens
//! were a flag: what another config layer contributes before this value is
//! outside the contract.

use crate::error::DevError;

/// One configured `strip_rustflags` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripSpec {
    /// The entry as written, for messages.
    raw: String,
    /// The entry's units, compared exactly.
    units: Vec<String>,
}

impl StripSpec {
    /// Parse one entry: a string split on whitespace, or an array of strings
    /// taken verbatim (for an operand containing whitespace).
    pub fn parse(value: &toml::Value) -> Result<Self, DevError> {
        let (raw, tokens): (String, Vec<String>) = match value {
            toml::Value::String(s) => (
                s.clone(),
                s.split_whitespace().map(str::to_owned).collect(),
            ),
            toml::Value::Array(items) => {
                let tokens = items
                    .iter()
                    .map(|item| item.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| {
                        DevError::Config(
                            "strip_rustflags: an array entry must hold only strings"
                                .into(),
                        )
                    })?;
                (format!("{tokens:?}"), tokens)
            }
            other => {
                return Err(DevError::Config(format!(
                    "strip_rustflags entries are strings or arrays of strings (got {other})"
                )));
            }
        };
        if tokens.is_empty() || tokens.iter().any(String::is_empty) {
            return Err(DevError::Config(format!(
                "strip_rustflags entry {raw:?} is empty or holds an empty token"
            )));
        }
        let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
        let units = units(&refs).into_iter().map(|unit| unit.text).collect();
        Ok(Self { raw, units })
    }

    /// The entry as written.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Whether two entries name the same unit sequence (`-Cx` and `-C x`).
    pub fn same_as(&self, other: &Self) -> bool {
        self.units == other.units
    }
}

/// A unit and the physical element indices it spans.
#[derive(Debug)]
struct Unit {
    text: String,
    start: usize,
    end: usize,
}

fn units(elements: &[&str]) -> Vec<Unit> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < elements.len() {
        let element = elements[i];
        if (element == "-C" || element == "-Z") && i + 1 < elements.len() {
            out.push(Unit {
                text: format!("{element}{}", elements[i + 1]),
                start: i,
                end: i + 2,
            });
            i += 2;
        } else {
            out.push(Unit {
                text: element.to_owned(),
                start: i,
                end: i + 1,
            });
            i += 1;
        }
    }
    out
}

/// Short options that take no operand.
const OPERANDLESS_SHORT: &[&str] = &["-O", "-g", "-v", "-V", "-h"];
/// Long options that take no operand.
const OPERANDLESS_LONG: &[&str] = &["--test", "--verbose", "--version", "--help"];

/// Whether `unit` is an option that may consume the following element as its
/// operand: a bare short option, or a long option without `=`, outside the
/// known operand-less sets.
fn may_take_operand(unit: &str) -> bool {
    let bytes = unit.as_bytes();
    if bytes.len() == 2 && bytes[0] == b'-' && bytes[1].is_ascii_alphabetic() {
        return !OPERANDLESS_SHORT.contains(&unit);
    }
    unit.len() > 2 && unit.starts_with("--") && !unit.contains('=') && !OPERANDLESS_LONG.contains(&unit)
}

/// What filtering one value found.
#[derive(Debug, Default)]
pub struct ValueEdit {
    /// Physical element indices to delete, ascending.
    pub removed: Vec<usize>,
    /// Per-spec match counts, parallel to the spec slice.
    pub counts: Vec<usize>,
}

/// Match `specs` against one value's elements. `key` names the value in
/// refusals.
pub fn match_value(elements: &[&str], specs: &[StripSpec], key: &str) -> Result<ValueEdit, DevError> {
    let units = units(elements);
    let mut counts = vec![0; specs.len()];
    // (first unit, unit count, spec index), in discovery order.
    let mut matches: Vec<(usize, usize, usize)> = Vec::new();
    for (spec_index, spec) in specs.iter().enumerate() {
        let len = spec.units.len();
        if len > units.len() {
            continue;
        }
        for start in 0..=units.len() - len {
            if units[start..start + len]
                .iter()
                .zip(&spec.units)
                .all(|(unit, want)| unit.text == *want)
            {
                matches.push((start, len, spec_index));
            }
        }
    }
    matches.sort_unstable();
    for pair in matches.windows(2) {
        let (a_start, a_len, a_spec) = pair[0];
        let (b_start, _, b_spec) = pair[1];
        if b_start < a_start + a_len {
            return Err(DevError::Config(format!(
                "strip_rustflags entries {:?} and {:?} overlap in {key}; \
                 brokkr will not choose between them",
                specs[a_spec].raw, specs[b_spec].raw
            )));
        }
    }
    let mut removed = Vec::new();
    for &(start, len, spec_index) in &matches {
        let spec = &specs[spec_index].raw;
        // Checked on the physical elements: grouping would hide a response
        // file behind a `-C` (`-C @args` is the unit `-C@args`).
        if let Some(barrier) = elements[..units[start].start]
            .iter()
            .find(|element| element.starts_with('@') || **element == "--")
        {
            return Err(DevError::Refused(format!(
                "strip_rustflags {spec:?} matches in {key} after `{barrier}`, \
                 which changes how the following tokens are read; brokkr will not edit that value"
            )));
        }
        if let Some(previous) = start.checked_sub(1).map(|i| &units[i])
            && may_take_operand(&previous.text)
        {
            return Err(DevError::Refused(format!(
                "strip_rustflags {spec:?} matches in {key} right after `{}`, \
                 which may take it as an operand; brokkr will not edit that value",
                previous.text
            )));
        }
        counts[spec_index] += 1;
        for unit in &units[start..start + len] {
            removed.extend(unit.start..unit.end);
        }
    }
    Ok(ValueEdit { removed, counts })
}

/// The outcome of filtering one config document.
#[derive(Debug)]
pub struct DocumentEdit {
    /// The rewritten document text, or `None` when nothing matched.
    pub text: Option<String>,
    /// Per-spec match counts across the document.
    pub counts: Vec<usize>,
    /// Whether the document has an `include`, named in the nothing-matched
    /// warning as a place the flag might live instead.
    pub has_include: bool,
    /// Whether some rustflags value holds a response file (`@path`).
    pub has_response_file: bool,
}

/// Filter every rustflags value of a cargo config document: `build.rustflags`,
/// `host.rustflags`, `host.<triple>.rustflags` and `target.<any>.rustflags`.
/// Only values with a match are rewritten, each keeping its type and decor.
pub fn filter_document(text: &str, specs: &[StripSpec], file: &str) -> Result<DocumentEdit, DevError> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|error| {
        DevError::Config(format!("could not parse Cargo config {file}: {error}"))
    })?;
    let mut counts = vec![0; specs.len()];
    let mut has_response_file = false;
    let mut changed = false;
    let has_include = doc.contains_key("include");

    let mut tables: Vec<(String, Vec<String>)> = vec![
        ("build".into(), vec!["build".into()]),
        ("host".into(), vec!["host".into()]),
    ];
    for parent in ["host", "target"] {
        if let Some(table) = doc.get(parent).and_then(toml_edit::Item::as_table_like) {
            for (name, item) in table.iter() {
                if item.is_table_like() {
                    tables.push((format!("{parent}.{name}"), vec![parent.into(), name.into()]));
                }
            }
        }
    }

    for (label, path) in tables {
        let key = format!("{label}.rustflags in {file}");
        let Some(value) = rustflags_value(&mut doc, &path) else {
            continue;
        };
        let edit = match value {
            toml_edit::Value::String(s) => {
                let original = s.value().clone();
                let elements: Vec<&str> = original.split_whitespace().collect();
                has_response_file |= elements.iter().any(|e| e.starts_with('@'));
                let edit = match_value(&elements, specs, &key)?;
                if !edit.removed.is_empty() {
                    let kept: Vec<&str> = elements
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !edit.removed.contains(i))
                        .map(|(_, e)| *e)
                        .collect();
                    let decor = value.decor().clone();
                    *value = toml_edit::Value::from(kept.join(" "));
                    *value.decor_mut() = decor;
                }
                edit
            }
            toml_edit::Value::Array(array) => {
                let Some(elements) = array
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<Vec<String>>>()
                else {
                    // Not a rustflags shape cargo accepts; cargo reports it.
                    continue;
                };
                let refs: Vec<&str> = elements.iter().map(String::as_str).collect();
                has_response_file |= refs.iter().any(|e| e.starts_with('@'));
                let edit = match_value(&refs, specs, &key)?;
                for &index in edit.removed.iter().rev() {
                    array.remove(index);
                }
                edit
            }
            _ => continue,
        };
        changed |= !edit.removed.is_empty();
        for (total, count) in counts.iter_mut().zip(&edit.counts) {
            *total += count;
        }
    }

    Ok(DocumentEdit {
        text: changed.then(|| doc.to_string()),
        counts,
        has_include,
        has_response_file,
    })
}

/// The `rustflags` value under the table at `path`, when it is a plain value.
fn rustflags_value<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    path: &[String],
) -> Option<&'a mut toml_edit::Value> {
    let mut item: &mut toml_edit::Item = doc.as_item_mut();
    for segment in path {
        item = item.as_table_like_mut()?.get_mut(segment)?;
    }
    item.as_table_like_mut()?.get_mut("rustflags")?.as_value_mut()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn spec(s: &str) -> StripSpec {
        StripSpec::parse(&toml::Value::String(s.into())).unwrap()
    }

    const NAUTILUS: &str = r#"[build]
warnings = "deny"

[target.'cfg(all())']
rustflags = ["-Dwarnings"]

[target.'cfg(target_os = "linux")']
rustflags = [
  "-C",
  "link-arg=-fuse-ld=lld",
  "-C",
  "link-arg=-Wl,--gc-sections",
]
"#;

    #[test]
    fn strips_a_split_pair_and_keeps_the_rest_of_the_file() {
        for form in ["-Clink-arg=-fuse-ld=lld", "-C link-arg=-fuse-ld=lld"] {
            let edit = filter_document(NAUTILUS, &[spec(form)], "config.toml").unwrap();
            assert_eq!(edit.counts, vec![1]);
            assert_eq!(
                edit.text.unwrap(),
                NAUTILUS.replace("  \"-C\",\n  \"link-arg=-fuse-ld=lld\",\n", "")
            );
        }
    }

    #[test]
    fn a_payload_alone_never_matches_half_a_pair() {
        let edit = filter_document(NAUTILUS, &[spec("link-arg=-fuse-ld=lld")], "c").unwrap();
        assert_eq!(edit.counts, vec![0]);
        assert!(edit.text.is_none());
    }

    #[test]
    fn an_unmatched_document_is_not_rewritten_even_with_odd_spacing() {
        let text = "[build]\nrustflags = \"-Cfoo    -Cbar\"\n";
        let edit = filter_document(text, &[spec("-Cnope")], "c").unwrap();
        assert!(edit.text.is_none());
    }

    #[test]
    fn a_string_value_stays_a_string() {
        let text = "[build]\nrustflags = \"-Cfoo  -C bar -Cbaz\" # keep\n";
        let edit = filter_document(text, &[spec("-Cbar")], "c").unwrap();
        assert_eq!(edit.text.unwrap(), "[build]\nrustflags = \"-Cfoo -Cbaz\" # keep\n");
    }

    #[test]
    fn unmatched_values_are_untouched_when_another_matches() {
        let text = "[build]\nrustflags = [ \"-Cx\" ,\"-Cy\" ]\n[target.a]\nrustflags = [\"-Cz\"]\n";
        let edit = filter_document(text, &[spec("-Cz")], "c").unwrap();
        assert_eq!(
            edit.text.unwrap(),
            "[build]\nrustflags = [ \"-Cx\" ,\"-Cy\" ]\n[target.a]\nrustflags = []\n"
        );
    }

    #[test]
    fn host_tables_are_filtered() {
        let text = "[host]\nrustflags = [\"-Cx\"]\n[host.x86_64-unknown-linux-gnu]\nrustflags = [\"-Cx\", \"-Cy\"]\n";
        let edit = filter_document(text, &[spec("-Cx")], "c").unwrap();
        assert_eq!(edit.counts, vec![2]);
    }

    #[test]
    fn overlapping_specs_refuse() {
        let err = match_value(&["-Cx", "-Cy"], &[spec("-Cx"), spec("-Cx -Cy")], "k").unwrap_err();
        assert!(err.to_string().contains("overlap"), "{err}");
    }

    #[test]
    fn removal_does_not_create_new_matches() {
        let edit = match_value(&["-Ca", "-Cb", "-Ca"], &[spec("-Cb"), spec("-Ca -Ca")], "k").unwrap();
        assert_eq!(edit.removed, vec![1]);
    }

    #[test]
    fn barriers_and_operand_takers_refuse() {
        for elements in [
            vec!["@args", "-Cx"],
            vec!["-C", "@args", "-Cx"],
            vec!["--", "-Cx"],
            vec!["-Z", "--", "-Cx"],
            vec!["-o", "-Cx"],
            vec!["--emit", "-Cx"],
        ] {
            assert!(match_value(&elements, &[spec("-Cx")], "k").is_err(), "{elements:?}");
        }
        for elements in [vec!["-O", "-Cx"], vec!["--test", "-Cx"], vec!["--cfg=foo", "-Cx"]] {
            assert!(match_value(&elements, &[spec("-Cx")], "k").is_ok(), "{elements:?}");
        }
    }

    #[test]
    fn specs_reject_empty_and_accept_arrays() {
        for bad in [
            toml::Value::String("  ".into()),
            toml::Value::Array(vec![]),
            toml::Value::Array(vec![toml::Value::String(String::new())]),
            toml::Value::Integer(1),
        ] {
            assert!(StripSpec::parse(&bad).is_err(), "{bad:?}");
        }
        let array = StripSpec::parse(&toml::Value::Array(vec![
            toml::Value::String("-C".into()),
            toml::Value::String("link-arg=a b".into()),
        ]))
        .unwrap();
        assert_eq!(array.units, vec!["-Clink-arg=a b".to_owned()]);
        assert!(spec("-Cx").same_as(&spec("-C x")));
    }
}
