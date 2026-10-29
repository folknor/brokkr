//! Strict JSON for the library harness protocol.
//!
//! Every document of the protocol is one JSON object whose schema is closed:
//! an unknown, duplicate or missing key, a value of the wrong type and a
//! number with a fraction or an exponent are each refused, naming the path
//! into the document (`scenarios[2].roles[0].owner`). Serde derive cannot
//! say that: a derived struct also accepts a positional array, `Option`
//! makes an absent key and `null` one value, and its errors name a line and
//! column. Parsing into `serde_json::Value` first loses duplicate keys.
//!
//! So a document is parsed into [`Json`], which keeps every object member in
//! order and every integer as an integer, and read back through [`Obj`] and
//! [`Field`], which carry their path. Any other number is kept as
//! [`Json::NotInteger`], which no read accepts, so its refusal names a path
//! too, except a number beyond a double's range (`1e400`), which serde_json
//! refuses itself, at a line and column. An [`Obj`] records the keys it was
//! asked for; [`Obj::finish`] refuses the rest.

use std::cell::RefCell;
use std::collections::BTreeSet;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

/// One parsed JSON value. Objects keep duplicate keys, so the reader can
/// refuse them with a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Int(i128),
    /// A number that is not an integer in -2^63 to 2^64-1: a fraction, an
    /// exponent, `-0` or an integer out of range. No read accepts it; it is
    /// kept, not refused at the token, so the refusal can name its path.
    NotInteger,
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn kind(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "a boolean",
            Self::Int(_) => "an integer",
            Self::NotInteger => {
                "a number that is not an integer in -2^63 to 2^64-1 (a fraction, an exponent, \
                 -0, or out of range)"
            }
            Self::Str(_) => "a string",
            Self::Array(_) => "an array",
            Self::Object(_) => "an object",
        }
    }
}

impl<'de> serde::Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Int(i128::from(v)))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Int(i128::from(v)))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Json, E> {
        Ok(Json::NotInteger)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Json, E> {
        Ok(Json::Str(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Json, E> {
        Ok(Json::Str(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<Json>()? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut members = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Json>()? {
            members.push((key, value));
        }
        Ok(Json::Object(members))
    }
}

/// Parse `bytes` as one JSON document, `name` naming it in the error. The
/// whole input must be the document: only whitespace may follow it. A byte
/// order mark is not JSON and is refused.
pub(crate) fn parse(name: &str, bytes: &[u8]) -> Result<Json, String> {
    serde_json::from_slice::<Json>(bytes).map_err(|e| format!("{name} is not valid JSON: {e}"))
}

/// A located value: a value of the document and the path that reaches it.
pub(crate) struct Field<'a> {
    value: &'a Json,
    path: String,
}

impl<'a> Field<'a> {
    /// The document itself, named `name` in every path.
    pub(crate) fn root(value: &'a Json, name: &str) -> Self {
        Self {
            value,
            path: name.to_owned(),
        }
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    fn wrong(&self, expected: &str) -> String {
        format!(
            "{}: expected {expected}, found {}",
            self.path,
            self.value.kind()
        )
    }

    pub(crate) fn is_null(&self) -> bool {
        matches!(self.value, Json::Null)
    }

    pub(crate) fn as_str(&self) -> Result<&'a str, String> {
        match self.value {
            Json::Str(s) => Ok(s),
            _ => Err(self.wrong("a string")),
        }
    }

    /// An integer in `min..=max`.
    pub(crate) fn as_int_in(&self, min: i128, max: i128) -> Result<i128, String> {
        match self.value {
            Json::Int(n) if (min..=max).contains(n) => Ok(*n),
            Json::Int(n) => Err(format!("{}: {n} is outside {min} to {max}", self.path)),
            _ => Err(self.wrong("an integer")),
        }
    }

    /// The elements, each located as `path[i]`.
    pub(crate) fn as_array(&self) -> Result<Vec<Field<'a>>, String> {
        match self.value {
            Json::Array(items) => Ok(items
                .iter()
                .enumerate()
                .map(|(i, value)| Field {
                    value,
                    path: format!("{}[{i}]", self.path),
                })
                .collect()),
            _ => Err(self.wrong("an array")),
        }
    }

    /// The object, refusing a duplicate key.
    pub(crate) fn as_object(&self) -> Result<Obj<'a>, String> {
        let Json::Object(members) = self.value else {
            return Err(self.wrong("an object"));
        };
        let mut seen = BTreeSet::new();
        for (key, _) in members {
            if !seen.insert(key.as_str()) {
                return Err(format!("{}: duplicate key {key:?}", self.path));
            }
        }
        Ok(Obj {
            members,
            path: self.path.clone(),
            asked: RefCell::new(BTreeSet::new()),
        })
    }
}

/// An object whose keys are read by name. Every key read, present or not,
/// is recorded; [`Obj::finish`] refuses any other.
pub(crate) struct Obj<'a> {
    members: &'a [(String, Json)],
    path: String,
    asked: RefCell<BTreeSet<String>>,
}

impl<'a> Obj<'a> {
    /// The value at `key`, or `None` when the key is absent. A present `null`
    /// is `Some`.
    pub(crate) fn optional(&self, key: &str) -> Option<Field<'a>> {
        self.asked.borrow_mut().insert(key.to_owned());
        self.members
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, value)| Field {
                value,
                path: format!("{}.{key}", self.path),
            })
    }

    /// The value at `key`, refusing its absence.
    pub(crate) fn required(&self, key: &str) -> Result<Field<'a>, String> {
        self.optional(key)
            .ok_or_else(|| format!("{}: missing key {key:?}", self.path))
    }

    /// Refuse the presence of `key`, which this object's shape forbids;
    /// `why` says which shape.
    pub(crate) fn forbid(&self, key: &str, why: &str) -> Result<(), String> {
        match self.optional(key) {
            Some(found) => Err(format!("{}: not allowed {why}", found.path())),
            None => Ok(()),
        }
    }

    /// Refuse every key that was never asked for.
    pub(crate) fn finish(self) -> Result<(), String> {
        let asked = self.asked.borrow();
        match self.members.iter().find(|(k, _)| !asked.contains(k)) {
            Some((key, _)) => Err(format!("{}: unknown key {key:?}", self.path)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn root(text: &str) -> Json {
        parse("doc", text.as_bytes()).unwrap()
    }

    #[test]
    fn integers_stay_integers_and_every_other_number_is_refused() {
        assert_eq!(
            root("18446744073709551615"),
            Json::Int(i128::from(u64::MAX))
        );
        assert_eq!(
            root("-9223372036854775808"),
            Json::Int(i128::from(i64::MIN))
        );
        for text in ["600.0", "1e3", "1E3", "18446744073709551616", "-0.5", "-0"] {
            let doc = root(&format!("{{\"n\": {text}}}"));
            let top = Field::root(&doc, "doc");
            let obj = top.as_object().unwrap();
            let e = obj.required("n").unwrap().as_int_in(0, 9).unwrap_err();
            assert!(
                e.starts_with("doc.n: expected an integer, found a number that is not"),
                "{text}: {e}"
            );
        }
    }

    #[test]
    fn a_number_beyond_a_double_is_refused_by_the_parser_itself() {
        let e = parse("doc", b"{\"n\": 1e400}").unwrap_err();
        assert!(e.starts_with("doc is not valid JSON:"), "{e}");
    }

    #[test]
    fn only_whitespace_may_follow_the_document_and_a_bom_is_refused() {
        assert!(parse("doc", b"{} \n").is_ok());
        assert!(parse("doc", b"{} {}").is_err());
        assert!(parse("doc", b"\xEF\xBB\xBF{}").is_err());
    }

    #[test]
    fn duplicate_keys_survive_parsing_and_are_refused_with_their_path() {
        let doc = root(r#"{"a": {"b": 1, "b": 2}}"#);
        let top = Field::root(&doc, "doc");
        let obj = top.as_object().unwrap();
        let inner = obj.required("a").unwrap();
        let err = inner.as_object().err().unwrap();
        assert_eq!(err, "doc.a: duplicate key \"b\"");
    }

    #[test]
    fn an_array_is_never_an_object() {
        let doc = root("[1, 2]");
        let top = Field::root(&doc, "doc");
        assert_eq!(
            top.as_object().err().unwrap(),
            "doc: expected an object, found an array"
        );
    }

    #[test]
    fn absent_null_and_unknown_keys_are_told_apart() {
        let doc = root(r#"{"call": null, "extra": 1}"#);
        let top = Field::root(&doc, "doc");
        let obj = top.as_object().unwrap();
        assert!(obj.required("call").unwrap().is_null());
        assert!(obj.optional("owner").is_none());
        assert_eq!(obj.required("id").err().unwrap(), "doc: missing key \"id\"");
        assert_eq!(obj.finish().err().unwrap(), "doc: unknown key \"extra\"");
    }

    #[test]
    fn array_elements_carry_their_index() {
        let doc = root(r#"{"xs": [1, "two"]}"#);
        let top = Field::root(&doc, "doc");
        let obj = top.as_object().unwrap();
        let xs = obj.required("xs").unwrap();
        let items = xs.as_array().unwrap();
        assert_eq!(
            items[1].as_int_in(0, 9).err().unwrap(),
            "doc.xs[1]: expected an integer, found a string"
        );
    }
}
