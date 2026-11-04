//! JSON for script benches: `json.encode`, `json.decode` and `record`, over
//! the dellingr-free value tree the Lua seam (`super::lua`) converts to and
//! from.
//!
//! One shape everywhere (`docs/commands/bench.md`, The script's builtins): an
//! object has string keys, an array is a list, nothing nests deeper than
//! [`MAX_DEPTH`], a number is finite, a string is UTF-8. Decoding is strict
//! as the project's other JSON readers are: a key given twice in one object
//! is refused, before anything reaches Lua. Integers keep their exact value:
//! an integer token within 2^53 is a number, one beyond it (up to 64 bits) its
//! decimal text, as the resolved view hands TOML integers; a token with a
//! fraction or an exponent, or an integer past 64 bits, is a double.

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

use super::lua::Value;

/// The deepest a value may nest, the root being level 1: the bench store
/// refuses a record past 64 levels.
pub(crate) const MAX_DEPTH: usize = 64;

/// The largest text `json.encode` produces, dellingr's string limit.
pub(crate) const MAX_ENCODED: usize = 16 * 1024 * 1024;

/// 2^53: integers up to it in magnitude are exact as doubles.
const EXACT: f64 = 9_007_199_254_740_992.0;

/// `value` as JSON: object keys sorted, a whole number within 2^53 written
/// as an integer.
pub(crate) fn to_json(value: &Value) -> Result<serde_json::Value, String> {
    to_json_at(value, 1)
}

/// `level` is the nesting of the containers around `value`, the root's 1.
fn to_json_at(value: &Value, level: usize) -> Result<serde_json::Value, String> {
    if level > MAX_DEPTH && matches!(value, Value::List(_) | Value::Table(_)) {
        return Err(format!("nests deeper than {MAX_DEPTH} levels"));
    }
    Ok(match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Number(n) => number(*n)?,
        Value::Text(text) => serde_json::Value::String(text.clone()),
        Value::Handle(_) => {
            return Err("holds a handle: store or encode its path (h.path) instead".to_owned());
        }
        Value::List(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| to_json_at(item, level + 1))
                .collect::<Result<_, _>>()?,
        ),
        Value::Table(fields) => {
            let mut sorted: Vec<&(String, Value)> = fields.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let mut object = serde_json::Map::new();
            for (key, item) in sorted {
                object.insert(key.clone(), to_json_at(item, level + 1)?);
            }
            serde_json::Value::Object(object)
        }
    })
}

fn number(n: f64) -> Result<serde_json::Value, String> {
    if !n.is_finite() {
        return Err(format!("holds {n}, which JSON cannot"));
    }
    if n.fract() == 0.0 && n.abs() <= EXACT {
        #[allow(clippy::cast_possible_truncation)] // Whole and within 2^53.
        let int = n as i64;
        return Ok(serde_json::Value::from(int));
    }
    serde_json::Number::from_f64(n)
        .map(serde_json::Value::Number)
        .ok_or_else(|| format!("holds {n}, which JSON cannot"))
}

/// `json.encode`: compact text, refused past [`MAX_ENCODED`] bytes.
pub(crate) fn encode(value: &Value) -> Result<Vec<u8>, String> {
    let json = to_json(value)?;
    let mut out = Bounded(Vec::new());
    serde_json::to_writer(&mut out, &json).map_err(|e| {
        if out.0.len() >= MAX_ENCODED {
            format!("encodes to more than {MAX_ENCODED} bytes")
        } else {
            e.to_string()
        }
    })?;
    Ok(out.0)
}

/// A writer that refuses to grow past [`MAX_ENCODED`].
struct Bounded(Vec<u8>);

impl std::io::Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + buf.len() > MAX_ENCODED {
            self.0.resize(MAX_ENCODED, 0);
            return Err(std::io::Error::other("too large"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `record` as the store takes it: a JSON object at the root.
pub(crate) fn record(value: &Value) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    match value {
        Value::Table(_) => match to_json(value)? {
            serde_json::Value::Object(map) => Ok(map),
            _ => Err("must be a table with string keys".to_owned()),
        },
        Value::List(_) => Err("must be a table with string keys, not a list".to_owned()),
        Value::Null => Err("must be a table with string keys, not json.null".to_owned()),
        _ => Err("must be a table with string keys".to_owned()),
    }
}

/// `json.decode`: one JSON document, only whitespace after it.
pub(crate) fn decode(text: &[u8]) -> Result<Value, String> {
    let parsed: Parsed = serde_json::from_slice(text).map_err(|e| e.to_string())?;
    let value = parsed.0?;
    if depth(&value) > MAX_DEPTH {
        return Err(format!("nests deeper than {MAX_DEPTH} levels"));
    }
    Ok(value)
}

/// How many arrays and objects nest at the deepest point.
fn depth(value: &Value) -> usize {
    match value {
        Value::List(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
        Value::Table(fields) => 1 + fields.iter().map(|(_, v)| depth(v)).max().unwrap_or(0),
        _ => 0,
    }
}

/// A decoded value, or the strictness refusal found inside it (carried out
/// of the visitor so its text is ours, not serde's position-suffixed one).
struct Parsed(Result<Value, String>);

impl<'de> serde::Deserialize<'de> for Parsed {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ParsedVisitor)
    }
}

struct ParsedVisitor;

impl<'de> Visitor<'de> for ParsedVisitor {
    type Value = Parsed;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Parsed, E> {
        Ok(Parsed(Ok(Value::Null)))
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Parsed, E> {
        Ok(Parsed(Ok(Value::Bool(v))))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Parsed, E> {
        Ok(Parsed(Ok(integer(i128::from(v)))))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Parsed, E> {
        Ok(Parsed(Ok(integer(i128::from(v)))))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Parsed, E> {
        Ok(Parsed(Ok(Value::Number(v))))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Parsed, E> {
        Ok(Parsed(Ok(Value::Text(v.to_owned()))))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Parsed, E> {
        Ok(Parsed(Ok(Value::Text(v))))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Parsed, A::Error> {
        let mut items = Vec::new();
        let mut refused = None;
        while let Some(item) = seq.next_element::<Parsed>()? {
            match item.0 {
                Ok(value) => items.push(value),
                Err(why) => refused = refused.or(Some(why)),
            }
        }
        Ok(Parsed(match refused {
            Some(why) => Err(why),
            None => Ok(Value::List(items)),
        }))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Parsed, A::Error> {
        let mut fields: Vec<(String, Value)> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut refused = None;
        while let Some((key, item)) = map.next_entry::<String, Parsed>()? {
            if !seen.insert(key.clone()) {
                refused = refused
                    .or_else(|| Some(format!("the key {key:?} is given twice in one object")));
            }
            match item.0 {
                Ok(value) => fields.push((key, value)),
                Err(why) => refused = refused.or(Some(why)),
            }
        }
        Ok(Parsed(match refused {
            Some(why) => Err(why),
            None => Ok(Value::Table(fields)),
        }))
    }
}

/// An exact integer token: a number within 2^53, else its decimal text.
fn integer(n: i128) -> Value {
    #[allow(clippy::cast_precision_loss)] // Within 2^53, exact.
    if n.unsigned_abs() <= 9_007_199_254_740_992 {
        Value::Number(n as f64)
    } else {
        Value::Text(n.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn decode_is_strict_and_keeps_integers_exact() {
        assert_eq!(
            decode(br#"{"a":[1,null,2.5],"b":"x","c":{}}"#).unwrap(),
            Value::Table(vec![
                (
                    "a".into(),
                    Value::List(vec![Value::Number(1.0), Value::Null, Value::Number(2.5)])
                ),
                ("b".into(), Value::Text("x".into())),
                ("c".into(), Value::Table(Vec::new())),
            ])
        );
        let err = decode(br#"{"a":null,"a":1}"#).unwrap_err();
        assert!(err.contains("given twice"), "{err}");
        assert_eq!(
            decode(b"9007199254740993").unwrap(),
            Value::Text("9007199254740993".into())
        );
        assert_eq!(decode(b"-12").unwrap(), Value::Number(-12.0));
        assert_eq!(
            decode(b"-9007199254740992").unwrap(),
            Value::Number(-9_007_199_254_740_992.0),
            "2^53 itself is exact"
        );
        assert_eq!(
            decode(b"18446744073709551616").unwrap(),
            Value::Number(18_446_744_073_709_551_616.0),
            "past 64 bits a double, as documented"
        );
        assert!(decode(b"[1] x").is_err());
        let deep = format!("{}{}", "[".repeat(65), "]".repeat(65));
        assert!(
            decode(deep.as_bytes())
                .unwrap_err()
                .contains("deeper than 64")
        );
        let ok = format!("{}{}", "[".repeat(64), "]".repeat(64));
        assert!(decode(ok.as_bytes()).is_ok());
    }

    #[test]
    fn encode_sorts_keys_and_refuses_what_json_cannot_hold() {
        let value = Value::Table(vec![
            ("b".into(), Value::List(Vec::new())),
            ("a".into(), Value::Number(3.0)),
            ("c".into(), Value::Null),
            ("d".into(), Value::Number(0.5)),
        ]);
        assert_eq!(
            encode(&value).unwrap(),
            br#"{"a":3,"b":[],"c":null,"d":0.5}"#
        );
        assert!(encode(&Value::Number(f64::NAN)).is_err());
        let handle = Value::Handle(super::super::lua::HandleRef {
            index: 0,
            path: "/x".into(),
        });
        assert!(encode(&handle).unwrap_err().contains("h.path"));
        let big = Value::Text("\u{1}".repeat(MAX_ENCODED / 4));
        assert!(encode(&big).unwrap_err().contains("more than"));
    }

    #[test]
    fn record_is_an_object_at_the_root() {
        assert!(record(&Value::Table(Vec::new())).unwrap().is_empty());
        assert!(
            record(&Value::List(Vec::new()))
                .unwrap_err()
                .contains("not a list")
        );
        assert!(record(&Value::Null).unwrap_err().contains("json.null"));
    }
}
