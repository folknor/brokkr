//! A bench's annotations: `description`, `version`, `date` and `changelog`,
//! notes devs keep for each other. brokkr reads nothing into them: it
//! checks their shape, shows them, and names when two stored runs' differ.
//!
//! One reader serves both the live config, where an invalid field refuses
//! the load, and a stored run's config, where it is named beside the
//! comparison. Each field is read on its own, so a malformed one never
//! hides a valid sibling.

use toml::value::Datetime;

/// The keys this module owns in a `[bench.NAME]` table.
pub const KEYS: [&str; 4] = ["description", "version", "date", "changelog"];

/// One field as read: absent, present but not what the field takes (why),
/// or its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field<T> {
    Absent,
    Invalid(String),
    Value(T),
}

impl<T> Field<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Value(v) => Some(v),
            _ => None,
        }
    }
}

/// A bench's annotations, every field optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotations {
    pub description: Field<String>,
    /// A positive integer the devs assign; brokkr orders nothing by it.
    pub version: Field<i64>,
    /// When the current version was set, as the devs say; never checked
    /// against the changelog.
    pub date: Field<Datetime>,
    /// In the order written; an absent changelog and an empty one differ.
    pub changelog: Field<Vec<Entry>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub version: Option<i64>,
    pub date: Option<Datetime>,
    pub note: String,
}

impl Annotations {
    #[cfg(test)]
    pub const NONE: Self = Self {
        description: Field::Absent,
        version: Field::Absent,
        date: Field::Absent,
        changelog: Field::Absent,
    };

    /// Read the annotation keys of a bench table, ignoring every other key.
    pub fn read(table: &toml::Table) -> Self {
        Self {
            description: read_field(table.get("description"), |v| nonblank(v, "description")),
            version: read_field(table.get("version"), |v| positive(v, "version")),
            date: read_field(table.get("date"), |v| local_date(v, "date")),
            changelog: read_field(table.get("changelog"), changelog),
        }
    }

    /// Every invalid field's reason, for a load refusal.
    pub fn problems(&self) -> Vec<String> {
        [
            invalid(&self.description),
            invalid(&self.version),
            invalid(&self.date),
            invalid(&self.changelog),
        ]
        .into_iter()
        .flatten()
        .map(str::to_owned)
        .collect()
    }
}

fn invalid<T>(field: &Field<T>) -> Option<&str> {
    match field {
        Field::Invalid(why) => Some(why),
        _ => None,
    }
}

fn read_field<T>(value: Option<&toml::Value>, parse: impl Fn(&toml::Value) -> Result<T, String>) -> Field<T> {
    match value {
        None => Field::Absent,
        Some(v) => match parse(v) {
            Ok(v) => Field::Value(v),
            Err(why) => Field::Invalid(why),
        },
    }
}

fn type_name(value: &toml::Value) -> &'static str {
    value.type_str()
}

fn nonblank(value: &toml::Value, key: &str) -> Result<String, String> {
    match value {
        toml::Value::String(s) if !s.trim().is_empty() => Ok(s.clone()),
        toml::Value::String(_) => Err(format!("{key} is blank")),
        other => Err(format!("{key} must be a string, not {}", type_name(other))),
    }
}

fn positive(value: &toml::Value, key: &str) -> Result<i64, String> {
    match value {
        toml::Value::Integer(n) if *n > 0 => Ok(*n),
        toml::Value::Integer(n) => Err(format!("{key} {n} is not a positive integer")),
        other => Err(format!(
            "{key} must be a positive integer, not {}",
            type_name(other)
        )),
    }
}

/// A TOML local date, written bare (`2026-11-08`): no time, no offset, and
/// not a quoted string.
fn local_date(value: &toml::Value, key: &str) -> Result<Datetime, String> {
    match value {
        toml::Value::Datetime(dt) if dt.date.is_some() && dt.time.is_none() && dt.offset.is_none() => {
            Ok(*dt)
        }
        toml::Value::Datetime(dt) => Err(format!(
            "{key} {dt} is not a local date: write the day alone, unquoted (2026-11-08)"
        )),
        other => Err(format!(
            "{key} must be a local date, unquoted (2026-11-08), not {}",
            type_name(other)
        )),
    }
}

fn changelog(value: &toml::Value) -> Result<Vec<Entry>, String> {
    let toml::Value::Array(items) = value else {
        return Err(format!(
            "changelog must be an array of {{ version, date, note }} tables, not {}",
            type_name(value)
        ));
    };
    let mut out = Vec::with_capacity(items.len());
    let mut problems = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        match entry(item) {
            Ok(entry) => out.push(entry),
            Err(why) => problems.push(format!("changelog entry {idx}: {why}")),
        }
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(problems.join("; "))
    }
}

fn entry(item: &toml::Value) -> Result<Entry, String> {
    let toml::Value::Table(table) = item else {
        return Err(format!("must be a table, not {}", type_name(item)));
    };
    if let Some(unknown) = table
        .keys()
        .find(|k| !matches!(k.as_str(), "version" | "date" | "note"))
    {
        return Err(format!(
            "unknown key {unknown:?}; an entry takes version, date and note"
        ));
    }
    let note = match table.get("note") {
        None => return Err("needs a note".into()),
        Some(v) => nonblank(v, "note")?,
    };
    Ok(Entry {
        version: table.get("version").map(|v| positive(v, "version")).transpose()?,
        date: table.get("date").map(|v| local_date(v, "date")).transpose()?,
        note,
    })
}

impl Annotations {
    /// The `--help` block, each line indented by two spaces and every
    /// continuation line of a multi-line text under its field's text. Empty
    /// when the bench has none; only valid fields are shown, since a live
    /// bench's are all valid.
    pub fn help_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(text) = self.description.value() {
            push_text(&mut out, "  ", text);
        }
        let version = self.version.value().map(|v| format!("version {v}"));
        let date = self.date.value().map(|d| format!("dated {d}"));
        let stamp: Vec<String> = version.into_iter().chain(date).collect();
        if !stamp.is_empty() {
            out.push(format!("  {}", stamp.join(", ")));
        }
        if let Some(entries) = self.changelog.value() {
            if entries.is_empty() {
                out.push("  changelog: (empty)".into());
            } else {
                out.push("  changelog:".into());
            }
            for entry in entries {
                let mut head = String::from("    -");
                if let Some(v) = entry.version {
                    head.push_str(&format!(" v{v}"));
                }
                if let Some(d) = entry.date {
                    head.push_str(&format!(" {d}"));
                }
                head.push(' ');
                push_text(&mut out, &head, &entry.note);
            }
        }
        out
    }
}

/// `text` after `lead` on its first line, every later line indented to the
/// text's column; blank lines stay blank.
fn push_text(out: &mut Vec<String>, lead: &str, text: &str) {
    let indent = " ".repeat(lead.chars().count());
    for (idx, line) in text.trim_end().lines().enumerate() {
        let line = line.trim_end();
        out.push(if idx == 0 {
            format!("{lead}{line}")
        } else if line.is_empty() {
            String::new()
        } else {
            format!("{indent}{line}")
        });
    }
}

/// The first nonblank line of `text`, trimmed: what a one-line listing shows.
pub fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn read(text: &str) -> Annotations {
        Annotations::read(&toml::from_str(text).unwrap())
    }

    #[test]
    fn every_field_reads_and_others_are_ignored() {
        let a = read(
            "description = \"d\"\nversion = 3\ndate = 2026-11-08\nscript = \"x\"\n\
             changelog = [{ version = 3, date = 2026-11-08, note = \"n\" }, { note = \"m\" }]\n",
        );
        assert_eq!(a.description, Field::Value("d".into()));
        assert_eq!(a.version, Field::Value(3));
        assert_eq!(a.date.value().unwrap().to_string(), "2026-11-08");
        let log = a.changelog.value().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[1], Entry { version: None, date: None, note: "m".into() });
        assert!(a.problems().is_empty());
        assert_eq!(read("script = \"x\"\n"), Annotations::NONE);
    }

    #[test]
    fn an_empty_changelog_is_not_an_absent_one() {
        assert_eq!(read("changelog = []\n").changelog, Field::Value(Vec::new()));
        assert_eq!(read("").changelog, Field::Absent);
    }

    #[test]
    fn each_invalid_field_is_named_and_spares_its_siblings() {
        for (text, needle) in [
            ("description = \" \"", "blank"),
            ("description = 1", "must be a string"),
            ("version = 0", "not a positive integer"),
            ("version = \"3\"", "positive integer"),
            ("date = \"2026-11-08\"", "local date"),
            ("date = 2026-11-08T10:00:00", "not a local date"),
            ("date = 2026-11-08T10:00:00Z", "not a local date"),
            ("date = 10:00:00", "not a local date"),
            ("changelog = 1", "array"),
            ("changelog = [{ version = 1 }]", "entry 0: needs a note"),
            ("changelog = [{ note = \"a\" }, { note = \"b\", by = \"x\" }]", "entry 1: unknown key"),
            ("changelog = [{ note = \"a\", version = -1 }]", "not a positive integer"),
            ("changelog = [\"a\"]", "must be a table"),
        ] {
            let a = read(text);
            let problems = a.problems();
            assert_eq!(problems.len(), 1, "{text}: {problems:?}");
            assert!(problems[0].contains(needle), "{text}: {problems:?}");
        }
        let a = read("version = 0\ndescription = \"kept\"\n");
        assert_eq!(a.description, Field::Value("kept".into()));
    }

    #[test]
    fn help_indents_multiline_text_under_its_field() {
        let a = read(
            "description = \"Decode.\\nAll of it.\"\nversion = 2\ndate = 2026-11-08\n\
             changelog = [{ version = 2, date = 2026-11-08, note = \"Two\\nlines\" }, { note = \"x\" }]\n",
        );
        assert_eq!(
            a.help_lines(),
            [
                "  Decode.",
                "  All of it.",
                "  version 2, dated 2026-11-08",
                "  changelog:",
                "    - v2 2026-11-08 Two",
                "                    lines",
                "    - x",
            ]
        );
        assert!(read("").help_lines().is_empty());
        assert_eq!(read("changelog = []").help_lines(), ["  changelog: (empty)"]);
    }

    #[test]
    fn the_first_line_skips_blank_ones() {
        assert_eq!(first_line("\n  Useful\nmore"), "Useful");
        assert_eq!(first_line("one"), "one");
    }
}
