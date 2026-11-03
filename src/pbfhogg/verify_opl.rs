//! A strict streaming reader for the relation records of an OPL file osmium
//! wrote (`osmium cat -t relation -f opl`): each relation's ID and its typed
//! member references, in member order. Tags, roles and metadata are not read.
//!
//! OPL is one object per line, fields separated by one space, each field a
//! letter followed by its value; the member field is `M` followed by
//! comma-separated `<type><ref>@<role>` tokens, `n`, `w` or `r` for the type.
//! osmium escapes a space, comma, `@` and the other delimiters inside a value
//! as `%hex%`, so splitting on space and comma never cuts a value. The reader
//! is strict because what it yields decides a verdict: a record that is not a
//! relation, a line without its terminating newline (a truncated file), a
//! record with no member field or more than one, a member token it cannot
//! read, and an ID that does not strictly ascend are each an error, never a
//! record silently skipped. An `M` with nothing after it is a relation with
//! no members.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::error::DevError;
use crate::osc::OscKind;

/// One relation record: its ID and its members as `(kind, ref)`.
pub(super) struct OplRelation {
    pub id: i64,
    pub members: Vec<(OscKind, i64)>,
}

/// Hand every relation record of the OPL file at `path` to `each`, in file
/// order; returns the record count. IDs must strictly ascend.
pub(super) fn scan_relations(
    path: &Path,
    mut each: impl FnMut(&OplRelation),
) -> Result<u64, DevError> {
    let file = File::open(path)
        .map_err(|e| DevError::Verify(format!("cannot open OPL {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut records: u64 = 0;
    let mut last: Option<i64> = None;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| DevError::Verify(format!("cannot read OPL {}: {e}", path.display())))?;
        if read == 0 {
            return Ok(records);
        }
        let at = |why: String| {
            DevError::Verify(format!(
                "OPL {} record {}: {why}",
                path.display(),
                records + 1
            ))
        };
        let Some(body) = line.strip_suffix(b"\n") else {
            return Err(at("no terminating newline (truncated?)".into()));
        };
        let relation = parse_relation(body).map_err(at)?;
        if last.is_some_and(|last| relation.id <= last) {
            return Err(at(format!(
                "relation {} does not ascend from {}",
                relation.id,
                last.unwrap_or_default()
            )));
        }
        last = Some(relation.id);
        records += 1;
        each(&relation);
    }
}

/// One OPL line (without its newline) as a relation record.
fn parse_relation(line: &[u8]) -> Result<OplRelation, String> {
    let mut fields = line.split(|&b| b == b' ');
    let first = fields.next().unwrap_or_default();
    let id = match first.split_first() {
        Some((b'r', digits)) => parse_id(digits).ok_or_else(|| {
            format!(
                "relation id {:?} is not a positive integer",
                String::from_utf8_lossy(digits)
            )
        })?,
        _ => {
            return Err(format!(
                "not a relation record: {:?}",
                String::from_utf8_lossy(first)
            ));
        }
    };
    let mut members = None;
    for field in fields {
        if let Some(list) = field.strip_prefix(b"M") {
            if members.is_some() {
                return Err(format!("relation {id} has more than one member field"));
            }
            members = Some(parse_members(list).map_err(|why| format!("relation {id}: {why}"))?);
        }
    }
    let members = members.ok_or_else(|| format!("relation {id} has no member field"))?;
    Ok(OplRelation { id, members })
}

/// The `M` field's value: comma-separated `<type><ref>@<role>` tokens, or
/// nothing at all.
fn parse_members(list: &[u8]) -> Result<Vec<(OscKind, i64)>, String> {
    if list.is_empty() {
        return Ok(Vec::new());
    }
    list.split(|&b| b == b',')
        .map(|token| {
            let bad = || {
                format!(
                    "member {:?} is not <type><ref>@<role>",
                    String::from_utf8_lossy(token)
                )
            };
            let (kind, rest) = token.split_first().ok_or_else(bad)?;
            let kind = match kind {
                b'n' => OscKind::Node,
                b'w' => OscKind::Way,
                b'r' => OscKind::Relation,
                _ => return Err(bad()),
            };
            let at = rest.iter().position(|&b| b == b'@').ok_or_else(bad)?;
            let id = parse_ref(&rest[..at]).ok_or_else(bad)?;
            Ok((kind, id))
        })
        .collect()
}

/// A member reference: any decimal integer osmium writes, a minus sign
/// allowed, zero too. A dangling or odd reference is still a reference the
/// relation carries, and one that never matches (a relation member, or a node
/// or way ID nothing holds) must not make the relation unreadable.
fn parse_ref(text: &[u8]) -> Option<i64> {
    let digits = text.strip_prefix(b"-").unwrap_or(text);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(text).ok()?.parse().ok()
}

/// A positive decimal ID: ASCII digits only, no sign, no leading zero.
fn parse_id(digits: &[u8]) -> Option<i64> {
    if digits.is_empty() || digits[0] == b'0' || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::unwrap_in_result, clippy::expect_used)]
    use super::*;

    type Record = (i64, Vec<(OscKind, i64)>);

    /// Write `text` to `file` in the calling test's own scratch `dir` and scan it.
    fn scan_text(dir: &Path, file: &str, text: &str) -> Result<Vec<Record>, DevError> {
        let path = dir.join(file);
        std::fs::write(&path, text).unwrap();
        let mut seen = Vec::new();
        let count = scan_relations(&path, |r| seen.push((r.id, r.members.clone())))?;
        assert_eq!(count, seen.len() as u64);
        Ok(seen)
    }

    #[test]
    fn reads_ids_and_typed_members_in_order() {
        let dir = crate::test_scratch::scratch("pbfhogg-verify-opl", "members");
        let seen = scan_text(
            &dir,
            "members.opl",
            "r5 v1 dV c1 t2020-01-01T00:00:00Z i1 ua%20%b Ttype=multipolygon,name=a%2c%b \
             Mw7@outer,n3@la%40%bel,r9@,w2@inner\nr6 v1 dV c0 t i0 u T M\nr8 M\
             n1@,r0@,r-2@\n",
        )
        .unwrap();
        assert_eq!(
            seen,
            vec![
                (
                    5,
                    vec![
                        (OscKind::Way, 7),
                        (OscKind::Node, 3),
                        (OscKind::Relation, 9),
                        (OscKind::Way, 2)
                    ]
                ),
                (6, vec![]),
                (
                    8,
                    vec![
                        (OscKind::Node, 1),
                        (OscKind::Relation, 0),
                        (OscKind::Relation, -2)
                    ]
                ),
            ]
        );
        assert!(scan_text(&dir, "empty.opl", "").unwrap().is_empty());
    }

    #[test]
    fn anything_it_cannot_account_for_is_an_error() {
        let dir = crate::test_scratch::scratch("pbfhogg-verify-opl", "refused");
        for (case, (text, why)) in [
            ("r5 v1 Mw7@\nr6 v1 Mn1@", "terminating newline"),
            ("n5 v1 x1 y2\n", "not a relation"),
            ("r5 v1 T\n", "no member field"),
            ("r5 Mw1@ Mw2@\n", "more than one"),
            ("r5 Mw1\n", "member"),
            ("r5 Mx1@\n", "member"),
            ("r5 Mw@\n", "member"),
            ("r5 Mw+1@\n", "member"),
            ("r5 Mw-@\n", "member"),
            ("r5 Mw1x@\n", "member"),
            ("r5 Mw1@,\n", "member"),
            ("r-5 M\n", "positive integer"),
            ("r6 M\nr6 M\n", "does not ascend"),
            ("r6 M\nr5 M\n", "does not ascend"),
        ]
        .into_iter()
        .enumerate()
        {
            let err = scan_text(&dir, &format!("{case}.opl"), text)
                .unwrap_err()
                .to_string();
            assert!(err.contains(why), "{text:?}: {err}");
        }
    }
}
