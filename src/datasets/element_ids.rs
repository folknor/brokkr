//! Per-dataset OSM element ID lists: the `getid_ids` and `getparents_ids`
//! keys of a `[<host>.datasets.<name>]` block.
//!
//! The getid and getparents benchmarks request a fixed ID list. The built-in
//! lists exist in Denmark; a dataset elsewhere names its own so its bench
//! times a real selection. Each entry is a kind letter (`n`, `w` or `r`) and a
//! non-negative 64-bit ID, the domain pbfhogg getid accepts. The list is parsed
//! at config load and rendered in one canonical spelling, so the argv and the
//! results pairing identity never depend on how the config spelled an ID.

use serde::{Deserialize, Deserializer, de};

/// One OSM element reference: kind letter and non-negative ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ElementId {
    kind: char,
    id: i64,
}

impl ElementId {
    fn parse(text: &str) -> Result<Self, String> {
        let mut chars = text.chars();
        let kind = match chars.next() {
            Some(kind @ ('n' | 'w' | 'r')) => kind,
            _ => {
                return Err(format!(
                    "element ID '{text}' must start with n, w or r (node, way, relation)"
                ));
            }
        };
        let digits = chars.as_str();
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "element ID '{text}' must be a kind letter followed by a non-negative decimal ID"
            ));
        }
        let id = digits
            .parse::<i64>()
            .map_err(|_| format!("element ID '{text}' is larger than a 64-bit OSM ID"))?;
        Ok(Self { kind, id })
    }
}

impl std::fmt::Display for ElementId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}{}", self.kind, self.id)
    }
}

/// A non-empty, duplicate-free list of element IDs, in configured order.
/// Order is kept: it is the argv order, and nothing proves it cannot matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ElementIdList(Vec<ElementId>);

impl ElementIdList {
    pub(crate) fn parse<S: AsRef<str>>(entries: &[S]) -> Result<Self, String> {
        if entries.is_empty() {
            return Err("an element ID list must not be empty".into());
        }
        let mut ids: Vec<ElementId> = Vec::with_capacity(entries.len());
        for entry in entries {
            let id = ElementId::parse(entry.as_ref())?;
            if ids.contains(&id) {
                return Err(format!("element ID '{id}' is listed twice"));
            }
            ids.push(id);
        }
        Ok(Self(ids))
    }

    /// The canonical argv tokens (`n115722`, ...).
    pub(crate) fn args(&self) -> Vec<String> {
        self.0.iter().map(ToString::to_string).collect()
    }
}

impl<'de> Deserialize<'de> for ElementIdList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entries = Vec::<String>::deserialize(deserializer)?;
        Self::parse(&entries).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::ElementIdList;

    #[test]
    fn parses_and_renders_canonically() {
        let list = ElementIdList::parse(&["n1", "w007", "r0"]).unwrap();
        assert_eq!(list.args(), ["n1", "w7", "r0"]);
    }

    #[test]
    fn refuses_bad_entries() {
        let empty: [&str; 0] = [];
        assert!(ElementIdList::parse(&empty).is_err());
        for bad in [
            "x1",
            "n",
            "n-1",
            "n+1",
            "n1a",
            "N1",
            "n99999999999999999999",
            "",
        ] {
            assert!(ElementIdList::parse(&[bad]).is_err(), "{bad}");
        }
    }

    #[test]
    fn duplicates_are_compared_after_parsing() {
        let err = ElementIdList::parse(&["n1", "n01"]).unwrap_err();
        assert!(err.contains("listed twice"), "{err}");
        assert!(ElementIdList::parse(&["n1", "w1"]).is_ok());
    }
}
