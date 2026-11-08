//! The `annotations:` line of a comparison: how the compared bench's
//! description, version, date and changelog differ between the two runs'
//! stored configs. Descriptive only: brokkr reads nothing into a version
//! and claims no cause for a change in the numbers.
//!
//! Each side is read on its own from its stored config text
//! (`[config.bench.NAME]`), TOML or the JSON fallback, so one side's format
//! never decides the other's. The bench's table is found first and only its
//! annotation keys are read, each on its own, so nothing elsewhere in the
//! config and no other field can spoil one. A side whose text or table path
//! cannot be read is named unreadable, never taken for one without
//! annotations, and the other side is still shown. An invalid field is
//! named with its reason whenever either side holds one, equal or not.

use std::collections::BTreeMap;

use crate::config::bench_annotations::{Annotations, Field, KEYS};

/// One side's annotations: each present key's raw value (or why it has no
/// TOML form), which decides what differs, and their reading, which
/// decides how it is shown.
struct Side {
    raw: BTreeMap<&'static str, Result<toml::Value, String>>,
    notes: Annotations,
}

/// The key `toml`'s serializer gives a datetime it writes into a format
/// without one: a one-entry object holding the datetime's text.
const DATETIME_KEY: &str = "$__toml_private_datetime";

/// The step from one table to the next on the way to the bench's table:
/// absent, the table, or not a table.
enum Step<'a, T> {
    Absent,
    Table(&'a T),
    NotTable,
}

fn toml_step<'a>(table: &'a toml::Table, key: &str) -> Step<'a, toml::Table> {
    match table.get(key) {
        None => Step::Absent,
        Some(toml::Value::Table(t)) => Step::Table(t),
        Some(_) => Step::NotTable,
    }
}

fn json_step<'a>(
    map: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Step<'a, serde_json::Map<String, serde_json::Value>> {
    match map.get(key) {
        None => Step::Absent,
        Some(serde_json::Value::Object(m)) => Step::Table(m),
        Some(_) => Step::NotTable,
    }
}

/// Walk `config.bench.NAME`: `Ok(None)` when the bench (or every bench) is
/// absent, an error naming the first step that is not a table, or one with
/// no config table at all.
fn locate<'a, T>(
    root: &'a T,
    bench: &str,
    step: impl Fn(&'a T, &str) -> Step<'a, T>,
) -> Result<Option<&'a T>, String> {
    let config = match step(root, "config") {
        Step::Table(t) => t,
        Step::NotTable => return Err("its stored config's config is not a table".into()),
        Step::Absent => return Err("its stored config holds no config table".into()),
    };
    let benches = match step(config, "bench") {
        Step::Table(t) => t,
        Step::NotTable => return Err("its stored config's bench is not a table".into()),
        Step::Absent => return Ok(None),
    };
    match step(benches, bench) {
        Step::Table(t) => Ok(Some(t)),
        Step::NotTable => Err(format!("its stored config's bench.{bench} is not a table")),
        Step::Absent => Ok(None),
    }
}

fn read_side(config_text: &str, bench: &str) -> Result<Side, String> {
    let mut raw = BTreeMap::new();
    if let Ok(document) = toml::from_str::<toml::Table>(config_text) {
        if let Some(table) = locate(&document, bench, toml_step)? {
            for key in KEYS {
                if let Some(value) = table.get(key) {
                    raw.insert(key, Ok(value.clone()));
                }
            }
        }
    } else if let Ok(serde_json::Value::Object(document)) =
        serde_json::from_str::<serde_json::Value>(config_text)
    {
        if let Some(map) = locate(&document, bench, json_step)? {
            for key in KEYS {
                if let Some(value) = map.get(key) {
                    raw.insert(key, field_from_json(key, value));
                }
            }
        }
    } else {
        return Err("its stored config is readable as neither TOML nor JSON".into());
    }
    let readable: toml::Table = raw
        .iter()
        .filter_map(|(key, value)| Some(((*key).to_owned(), value.as_ref().ok()?.clone())))
        .collect();
    let mut notes = Annotations::read(&readable);
    for (key, value) in &raw {
        if let Err(why) = value {
            let why = format!("{key} {why}");
            match *key {
                "description" => notes.description = Field::Invalid(why),
                "version" => notes.version = Field::Invalid(why),
                "date" => notes.date = Field::Invalid(why),
                _ => notes.changelog = Field::Invalid(why),
            }
        }
    }
    Ok(Side { raw, notes })
}

/// One annotation key's JSON value as the TOML value it was serialized
/// from, a datetime wrapper decoded only where a date belongs: `date`
/// itself and a changelog entry's `date`.
fn field_from_json(key: &str, value: &serde_json::Value) -> Result<toml::Value, String> {
    match (key, value) {
        ("date", _) => date_from_json(value),
        ("changelog", serde_json::Value::Array(items)) => Ok(toml::Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(idx, item)| match item {
                    serde_json::Value::Object(entry) => {
                        let mut table = toml::Table::new();
                        for (k, v) in entry {
                            let v = if k == "date" {
                                date_from_json(v)
                            } else {
                                from_json(v)
                            };
                            let v = v.map_err(|why| format!("entry {idx}: {k} {why}"))?;
                            table.insert(k.clone(), v);
                        }
                        Ok(toml::Value::Table(table))
                    }
                    other => from_json(other).map_err(|why| format!("entry {idx} {why}")),
                })
                .collect::<Result<_, String>>()?,
        )),
        _ => from_json(value),
    }
}

fn date_from_json(value: &serde_json::Value) -> Result<toml::Value, String> {
    if let serde_json::Value::Object(map) = value
        && map.len() == 1
        && let Some(serde_json::Value::String(text)) = map.get(DATETIME_KEY)
        && let Ok(dt) = text.parse::<toml::value::Datetime>()
    {
        return Ok(toml::Value::Datetime(dt));
    }
    from_json(value)
}

/// A JSON value as TOML, every object a table. A null has no TOML form.
fn from_json(value: &serde_json::Value) -> Result<toml::Value, String> {
    Ok(match value {
        serde_json::Value::Null => return Err("holds a null, which TOML has no form for".into()),
        serde_json::Value::Bool(b) => toml::Value::Boolean(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => toml::Value::Integer(i),
            None => toml::Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        serde_json::Value::String(s) => toml::Value::String(s.clone()),
        serde_json::Value::Array(items) => {
            toml::Value::Array(items.iter().map(from_json).collect::<Result<_, _>>()?)
        }
        serde_json::Value::Object(map) => {
            let mut table = toml::Table::new();
            for (key, value) in map {
                table.insert(key.clone(), from_json(value)?);
            }
            toml::Value::Table(table)
        }
    })
}

fn invalid(why: &str) -> String {
    format!("(invalid: {why})")
}

fn shown<T: std::fmt::Display>(field: &Field<T>) -> String {
    match field {
        Field::Absent => "(none)".into(),
        Field::Invalid(why) => invalid(why),
        Field::Value(v) => v.to_string(),
    }
}

fn entries(notes: &Annotations) -> String {
    match &notes.changelog {
        Field::Value(list) => crate::output::count(list.len(), "entry"),
        Field::Invalid(why) => invalid(why),
        Field::Absent => "(none)".into(),
    }
}

fn description(notes: &Annotations) -> String {
    match &notes.description {
        Field::Value(_) => "set".into(),
        other => shown(other),
    }
}

fn is_invalid<T>(field: &Field<T>) -> bool {
    matches!(field, Field::Invalid(_))
}

/// One side alone, for when the other could not be read: its present
/// fields.
fn summary(side: &Side) -> String {
    let notes = &side.notes;
    let mut parts = Vec::new();
    if !matches!(notes.version, Field::Absent) {
        parts.push(format!("version {}", shown(&notes.version)));
    }
    if !matches!(notes.date, Field::Absent) {
        parts.push(format!("date {}", shown(&notes.date)));
    }
    if !matches!(notes.description, Field::Absent) {
        parts.push(format!("description {}", description(notes)));
    }
    if !matches!(notes.changelog, Field::Absent) {
        parts.push(format!("changelog {}", entries(notes)));
    }
    if parts.is_empty() {
        "no annotations".into()
    } else {
        parts.join(", ")
    }
}

fn differences(a: &Side, b: &Side) -> Vec<String> {
    let (x, y) = (&a.notes, &b.notes);
    let named = |key: &str, invalid: bool| invalid || a.raw.get(key) != b.raw.get(key);
    let mut parts = Vec::new();
    if named("version", is_invalid(&x.version) || is_invalid(&y.version)) {
        parts.push(format!("version {} -> {}", shown(&x.version), shown(&y.version)));
    }
    if named("date", is_invalid(&x.date) || is_invalid(&y.date)) {
        parts.push(format!("date {} -> {}", shown(&x.date), shown(&y.date)));
    }
    let bad = is_invalid(&x.description) || is_invalid(&y.description);
    if named("description", bad) {
        parts.push(match (&x.description, &y.description) {
            _ if bad => format!("description {} -> {}", description(x), description(y)),
            (Field::Absent, _) => "description added".into(),
            (_, Field::Absent) => "description removed".into(),
            _ => "description changed".into(),
        });
    }
    let bad = is_invalid(&x.changelog) || is_invalid(&y.changelog);
    if named("changelog", bad) {
        parts.push(match (&x.changelog, &y.changelog) {
            _ if bad => format!("changelog {} -> {}", entries(x), entries(y)),
            (Field::Absent, _) => format!("changelog added ({})", entries(y)),
            (_, Field::Absent) => format!("changelog removed (had {})", entries(x)),
            _ if entries(x) == entries(y) => format!("changelog changed ({})", entries(y)),
            _ => format!("changelog changed ({} -> {})", entries(x), entries(y)),
        });
    }
    parts
}

/// The line naming how bench `bench`'s annotations differ between two
/// stored configs, or `None` when both read, agree and hold nothing invalid.
pub(super) fn line(bench: &str, config_a: &str, config_b: &str) -> Option<String> {
    let body = match (read_side(config_a, bench), read_side(config_b, bench)) {
        (Ok(a), Ok(b)) => {
            let parts = differences(&a, &b);
            if parts.is_empty() {
                return None;
            }
            parts.join(", ")
        }
        (Err(why), Ok(b)) => format!("A unreadable ({why}); B {}", summary(&b)),
        (Ok(a), Err(why)) => format!("A {}; B unreadable ({why})", summary(&a)),
        (Err(x), Err(y)) => format!("A unreadable ({x}); B unreadable ({y})"),
    };
    Some(format!("  annotations: {body}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::line;

    fn stored(bench_body: &str) -> String {
        format!("[run]\nroot = \"/p\"\n[config.bench.cat]\nscript = \"x\"\n{bench_body}")
    }

    #[test]
    fn equal_or_absent_annotations_print_nothing() {
        let a = stored("version = 2\n");
        assert_eq!(line("cat", &a, &a), None);
        assert_eq!(line("cat", &stored(""), &stored("")), None);
        // Another key of the bench moving is config_diff's to name.
        assert_eq!(line("cat", &stored("target = \"a\""), &stored("target = \"b\"")), None);
    }

    #[test]
    fn each_field_difference_is_named() {
        let a = stored(
            "version = 2\ndate = 2026-09-14\ndescription = \"d\"\n\
             changelog = [{ note = \"one\" }, { note = \"two\" }]\n",
        );
        let b = stored(
            "version = 3\ndate = 2026-11-08\ndescription = \"e\"\n\
             changelog = [{ note = \"one\" }, { note = \"TWO\" }]\n",
        );
        assert_eq!(
            line("cat", &a, &b).unwrap(),
            "  annotations: version 2 -> 3, date 2026-09-14 -> 2026-11-08, description changed, \
             changelog changed (2 entries)"
        );
        let grown = stored("changelog = [{ note = \"one\" }]\n");
        let more = stored("changelog = [{ note = \"one\" }, { note = \"two\" }]\n");
        let out = line("cat", &grown, &more).unwrap();
        assert!(out.contains("changelog changed (1 entry -> 2 entries)"), "{out}");
        let added = line("cat", &stored(""), &stored("changelog = []\n")).unwrap();
        assert!(added.contains("changelog added (0 entries)"), "{added}");
        let removed = line("cat", &grown, &stored("")).unwrap();
        assert!(removed.contains("changelog removed (had 1 entry)"), "{removed}");
    }

    #[test]
    fn an_invalid_field_is_named_with_its_reason_even_when_equal() {
        let zero = stored("version = 0\n");
        let out = line("cat", &zero, &zero).unwrap();
        assert!(out.contains("version (invalid: version 0 is not a positive integer)"), "{out}");
        let out = line("cat", &stored(""), &stored("description = 7\n")).unwrap();
        assert!(out.contains("description (none) -> (invalid: description must be a string"), "{out}");
        let bad_log = stored("changelog = [{ version = 1 }]\n");
        let out = line("cat", &bad_log, &bad_log).unwrap();
        assert!(out.contains("changelog entry 0: needs a note"), "{out}");
        // An unchanged invalid field beside a changed valid one is named too.
        let out = line(
            "cat",
            &stored("version = 0\ndate = 2026-01-01\n"),
            &stored("version = 0\ndate = 2026-01-02\n"),
        )
        .unwrap();
        assert!(out.contains("version (invalid") && out.contains("date 2026-01-01"), "{out}");
    }

    #[test]
    fn an_unreadable_side_is_named_and_the_other_still_shown() {
        let b = stored("version = 3\ndate = 2026-11-08\n");
        let out = line("cat", "not a config", &b).unwrap();
        assert!(
            out.contains("A unreadable") && out.contains("B version 3, date 2026-11-08"),
            "{out}"
        );
        let broken = "[config]\nbench = 7\n";
        let out = line("cat", &b, broken).unwrap();
        assert!(out.contains("B unreadable") && out.contains("bench is not a table"), "{out}");
        let wrong = "[config.bench]\ncat = \"broken\"\n";
        assert!(line("cat", wrong, wrong).unwrap().contains("bench.cat is not a table"));
        assert!(line("cat", "[run]\n", "[run]\n").unwrap().contains("no config table"));
    }

    #[test]
    fn a_json_side_reads_its_dates_and_pairs_with_a_toml_side() {
        let toml_side = stored("version = 3\ndate = 2026-11-08\n");
        let json_side = r#"{"run":{},"config":{"bench":{"cat":{"script":"x","version":3,
            "date":{"$__toml_private_datetime":"2026-11-08"}}}}}"#;
        assert_eq!(line("cat", &toml_side, json_side), None);
        // A plain string is not a date, whatever it spells.
        let plain = r#"{"config":{"bench":{"cat":{"version":3,"date":"2026-11-08"}}}}"#;
        let out = line("cat", &toml_side, plain).unwrap();
        assert!(out.contains("date 2026-11-08 -> (invalid: date must be a local date"), "{out}");
    }

    /// A null elsewhere in a JSON side spoils nothing, and one in an
    /// annotation spoils that field alone; a datetime wrapper is decoded
    /// only where a date belongs.
    #[test]
    fn a_json_side_is_read_field_by_field() {
        let toml_side = stored("version = 3\n");
        let elsewhere = r#"{"config":{"data":{"x":null},"bench":{"cat":{"version":3}}}}"#;
        assert_eq!(line("cat", &toml_side, elsewhere), None);
        let in_field =
            r#"{"config":{"bench":{"cat":{"version":null,"description":"kept"}}}}"#;
        let out = line("cat", &stored("version = 3\ndescription = \"kept\"\n"), in_field).unwrap();
        assert!(out.contains("version 3 -> (invalid: version holds a null"), "{out}");
        assert!(!out.contains("description"), "{out}");
        let wrapped = r#"{"config":{"bench":{"cat":{
            "changelog":[{"$__toml_private_datetime":"2026-11-08"}]}}}}"#;
        let out = line("cat", &stored(""), wrapped).unwrap();
        assert!(out.contains("unknown key"), "{out}");
        let null_note = r#"{"config":{"bench":{"cat":{"changelog":[{"note":"v"},{"note":null}]}}}}"#;
        let out = line("cat", &stored(""), null_note).unwrap();
        assert!(out.contains("changelog entry 1: note holds a null"), "{out}");
        let null_entry = r#"{"config":{"bench":{"cat":{"changelog":[null]}}}}"#;
        let out = line("cat", &stored(""), null_entry).unwrap();
        assert!(out.contains("changelog entry 0 holds a null"), "{out}");
        let entry_date = r#"{"config":{"bench":{"cat":{
            "changelog":[{"note":"n","date":{"$__toml_private_datetime":"2026-11-08"}}]}}}}"#;
        assert_eq!(
            line("cat", &stored("changelog = [{ note = \"n\", date = 2026-11-08 }]\n"), entry_date),
            None
        );
    }
}
