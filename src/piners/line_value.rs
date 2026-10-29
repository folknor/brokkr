//! Read one harness NDJSON line into a `serde_json::Value` that knows which
//! keys repeated.
//!
//! `serde_json::from_str::<Value>` keeps the last of a repeated key with no
//! trace, so `{"outcome":"parity",...,"outcome":"compile_fail"}` would read
//! as one record. [`parse`] builds the same `Value` - the same visitor calls,
//! a repeated key's last value inserted into the same `serde_json::Map`, as
//! the `Value` deserializer does - and records the path of every key that repeats within
//! its object, at any depth, so the report parser can refuse a record the
//! harness did not write once. The library harness's strict reader
//! (`crate::pbfhogg::library`) keeps duplicates in its own tree instead, but
//! that tree drops float values a harness line needs for storage.

use std::cell::RefCell;
use std::fmt;

use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

/// A key that occurred more than once in one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatedKey {
    /// The key itself.
    pub key: String,
    /// The object is the line's own top level.
    pub top_level: bool,
    /// Where it is: each key as its JSON string literal, joined by `.`, an
    /// array index as `[i]` (`"acceptance"."tier"`, `"sites"[0]."name"`).
    pub path: String,
}

/// One parsed line.
#[derive(Debug)]
pub struct LineValue {
    pub value: Value,
    /// Every key that repeated, once per path, in the order first seen
    /// repeating.
    pub repeated: Vec<RepeatedKey>,
}

impl LineValue {
    /// Did this top-level key repeat?
    pub fn repeats_top(&self, key: &str) -> bool {
        self.repeated.iter().any(|r| r.top_level && r.key == key)
    }

    /// The repeated paths, comma separated.
    pub fn repeated_paths(&self) -> String {
        self.repeated
            .iter()
            .map(|r| r.path.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Parse `text` as exactly one JSON value: whitespace may follow it,
/// nothing else, as `serde_json::from_str` requires.
pub fn parse(text: &str) -> Result<LineValue, serde_json::Error> {
    let repeated = RefCell::new(Vec::new());
    let mut de = serde_json::Deserializer::from_str(text);
    let value = Seed {
        at: &At::Root,
        repeated: &repeated,
    }
    .deserialize(&mut de)?;
    de.end()?;
    Ok(LineValue {
        value,
        repeated: repeated.into_inner(),
    })
}

/// Where a value sits, as a chain to the root, rendered only when a key
/// repeats.
enum At<'a> {
    Root,
    Key(&'a At<'a>, &'a str),
    Index(&'a At<'a>, usize),
}

impl At<'_> {
    fn render(&self, out: &mut String) {
        match self {
            At::Root => {}
            At::Key(parent, key) => {
                parent.render(out);
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(&Value::from(*key).to_string());
            }
            At::Index(parent, index) => {
                parent.render(out);
                out.push_str(&format!("[{index}]"));
            }
        }
    }
}

struct Seed<'a, 'r> {
    at: &'a At<'a>,
    repeated: &'r RefCell<Vec<RepeatedKey>>,
}

impl<'de> DeserializeSeed<'de> for Seed<'_, '_> {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Seed<'_, '_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(Number::from_f64(v).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        self.deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        loop {
            let at = At::Index(self.at, items.len());
            let seed = Seed {
                at: &at,
                repeated: self.repeated,
            };
            match seq.next_element_seed(seed)? {
                Some(item) => items.push(item),
                None => break,
            }
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let at = At::Key(self.at, &key);
            let value = map.next_value_seed(Seed {
                at: &at,
                repeated: self.repeated,
            })?;
            if object.contains_key(&key) {
                let mut path = String::new();
                at.render(&mut path);
                let mut repeated = self.repeated.borrow_mut();
                if !repeated.iter().any(|r| r.path == path) {
                    repeated.push(RepeatedKey {
                        key: key.clone(),
                        top_level: matches!(self.at, At::Root),
                        path,
                    });
                }
            }
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_line_without_repeats_reads_as_serde_json_reads_it() {
        for text in [
            r#"{"probe":"p","outcome":"parity","runtime_ms":142.7,"matched":18446744073709551615,"n":-3,"z":null,"a":[1,{"b":true}],"s":"x\nA"}"#,
            "[]",
            "3",
            r#"{"x":1e300,"y":-0,"w":0.0}"#,
        ] {
            let ours = parse(text).unwrap();
            let theirs: Value = serde_json::from_str(text).unwrap();
            assert_eq!(ours.value, theirs, "{text}");
            assert_eq!(ours.value.to_string(), theirs.to_string(), "{text}");
            assert!(ours.repeated.is_empty(), "{text}");
        }
    }

    #[test]
    fn a_repeated_key_is_named_by_path_and_keeps_serde_jsons_collapse() {
        let text = r#"{"outcome":"parity","acceptance":{"tier":"a","tier":"b"},"sites":[{"n":1,"n":2}],"outcome":"compile_fail","outcome":"x"}"#;
        let ours = parse(text).unwrap();
        let theirs: Value = serde_json::from_str(text).unwrap();
        assert_eq!(ours.value.to_string(), theirs.to_string());
        let paths: Vec<&str> = ours.repeated.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                r#""acceptance"."tier""#,
                r#""sites"[0]."n""#,
                r#""outcome""#
            ]
        );
        assert!(ours.repeats_top("outcome"));
        assert!(!ours.repeats_top("tier"));
        assert!(!ours.repeats_top("n"));
    }

    #[test]
    fn a_nested_key_named_like_a_top_level_one_is_not_top_level() {
        let ours = parse(r#"{"x":{"kind":1,"kind":2},"kind":"disposition"}"#).unwrap();
        assert!(!ours.repeats_top("kind"));
        assert_eq!(ours.repeated[0].path, r#""x"."kind""#);
        // A key needing escapes is rendered as its literal, so a dot in it
        // cannot pass for a path separator.
        let dotted = parse(r#"{"a.b":1,"a.b":2}"#).unwrap();
        assert_eq!(dotted.repeated[0].path, r#""a.b""#);
    }

    #[test]
    fn trailing_input_and_syntax_errors_are_refused() {
        assert!(parse(r#"{"a":1} {"b":2}"#).is_err());
        assert!(parse(r#"{"a":1} x"#).is_err());
        assert!(parse(r#"{"a":1}   "#).is_ok());
        assert!(parse(r#"{"a":"#).is_err());
        assert!(parse("1e400").is_err());
    }
}
