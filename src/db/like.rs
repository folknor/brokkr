//! Literal `LIKE` patterns for prefix and substring matches.
//!
//! `LIKE` treats `%` and `_` in the *pattern* as wildcards, so binding a user
//! term straight into `col LIKE ?||'%'` quietly widens the match: a uuid or
//! commit prefix containing `_` matched any character there, and `--command
//! tags_filter` matched `tags-filter`. Every pattern built here escapes `\`,
//! `%` and `_`, and every call site must follow `LIKE ?N` with [`ESCAPE`] or
//! the escapes are themselves matched literally.
//!
//! `LIKE` rather than `instr`/`substr` because it keeps SQLite's ASCII
//! case-insensitivity, which `--command`/`--mode`/`--dataset` have always had.
//! The `--grep` terms use `instr` instead (see `query::grep_match_expr`) - those
//! are documented as case-sensitive literal substrings.

use crate::error::DevError;

/// The clause that must follow `LIKE ?N` whenever `?N` is bound to a pattern
/// from this module.
pub(crate) const ESCAPE: &str = "ESCAPE '\\'";

fn escape(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Refuse an empty (or all-whitespace) prefix. `prefix_pattern("")` is `%`,
/// which matches every row - fine for a filter nobody set, fatal for
/// `brokkr invalidate "" -f`, so a caller resolving a user-supplied prefix
/// checks here first. `what` names the thing ("uuid", "commit") in the error.
pub(crate) fn require_prefix(prefix: &str, what: &str) -> Result<(), DevError> {
    if prefix.trim().is_empty() {
        return Err(DevError::Config(format!(
            "empty {what} prefix would match every row - give at least one character"
        )));
    }
    Ok(())
}

/// Pattern matching any value that starts with `prefix`, literally.
pub(crate) fn prefix_pattern(prefix: &str) -> String {
    format!("{}%", escape(prefix))
}

/// Pattern matching any value that contains `term`, literally.
pub(crate) fn contains_pattern(term: &str) -> String {
    format!("%{}%", escape(term))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn matches(value: &str, pattern: &str) -> bool {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.query_row(
            &format!("SELECT ?1 LIKE ?2 {ESCAPE}"),
            rusqlite::params![value, pattern],
            |r| r.get::<_, bool>(0),
        )
        .unwrap()
    }

    #[test]
    fn underscore_and_percent_are_literal() {
        assert!(matches("tags_filter", &contains_pattern("s_f")));
        assert!(!matches("tags-filter", &contains_pattern("s_f")), "_ must not be a wildcard");
        assert!(matches("50%off", &prefix_pattern("50%")));
        assert!(!matches("500off", &prefix_pattern("50%")), "% must not be a wildcard");
        assert!(matches("a\\b", &prefix_pattern("a\\")), "the escape char itself is literal");
    }

    #[test]
    fn prefix_anchors_at_start() {
        assert!(matches("abc123", &prefix_pattern("abc")));
        assert!(!matches("xabc123", &prefix_pattern("abc")));
        assert!(matches("xabc123", &contains_pattern("abc")));
    }

    #[test]
    fn empty_prefix_is_refused() {
        assert!(require_prefix("", "uuid").is_err());
        assert!(require_prefix("  ", "uuid").is_err());
        assert!(require_prefix("a", "uuid").is_ok());
    }

    #[test]
    fn keeps_ascii_case_insensitivity() {
        assert!(matches("Tilegen", &contains_pattern("tilegen")));
    }
}
