//! Section addressing for `brokkr man`.
//!
//! A bundled doc is one file, but it is read one subject at a time: the
//! question is nearly always "what does `script_check` do", not "show me all
//! 890 lines of the config reference". This module gives every `##`/`###`
//! heading a slug and a byte range, so `brokkr man config script_check` can
//! print that section alone and bare `brokkr man config` can list what is
//! addressable.
//!
//! The slug is derived from the heading rather than declared, so it cannot
//! drift: renaming a heading renames its slug, and nothing else has to be
//! kept in sync. Matching is deliberately forgiving (exact, then prefix, then
//! substring) because the slug of a heading like ``## `[[script_check]]`
//! array`` is `script_check-array` and nobody is going to type that.

/// One addressable section: a heading and everything under it, up to the next
/// heading at the same or a shallower level (so a `##` carries its `###`
/// children along).
#[derive(Debug)]
pub(crate) struct Section {
    pub(crate) level: u8,
    pub(crate) slug: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Headings deeper than this are body text, not addressable subjects.
const MAX_LEVEL: u8 = 3;

/// Every `##`..`###` heading in `markdown`, in document order.
///
/// Fenced code blocks are skipped: a shell fence's `## note` comment is not a
/// heading, and treating it as one would slice a doc mid-example.
pub(crate) fn sections(markdown: &str) -> Vec<Section> {
    let mut heads: Vec<(u8, String, usize)> = Vec::new();
    let mut fence: Option<char> = None;
    let mut offset = 0usize;

    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim_start();
        match fence {
            Some(c) if trimmed.starts_with(c) && trimmed.starts_with(&repeat3(c)) => fence = None,
            Some(_) => {}
            None => {
                if trimmed.starts_with("```") {
                    fence = Some('`');
                } else if trimmed.starts_with("~~~") {
                    fence = Some('~');
                } else if let Some((level, title)) = parse_heading(trimmed) {
                    heads.push((level, title, offset));
                }
            }
        }
        offset += line.len();
    }

    let ends: Vec<usize> = heads
        .iter()
        .enumerate()
        .map(|(i, &(level, _, _))| {
            heads[i + 1..]
                .iter()
                .find(|&&(next, _, _)| next <= level)
                .map_or(markdown.len(), |&(_, _, start)| start)
        })
        .collect();

    heads
        .into_iter()
        .zip(ends)
        .map(|((level, title, start), end)| Section {
            level,
            slug: slugify(&title),
            start,
            end,
        })
        .collect()
}

fn repeat3(c: char) -> String {
    std::iter::repeat_n(c, 3).collect()
}

/// `## Title` -> `(2, "Title")`, for levels 2..=`MAX_LEVEL` only. A `#` with no
/// following space is not a heading (`#[derive]` in an unfenced snippet).
fn parse_heading(line: &str) -> Option<(u8, String)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    let level = u8::try_from(hashes).ok()?;
    if !(2..=MAX_LEVEL).contains(&level) {
        return None;
    }
    let rest = line.get(hashes..)?;
    let title = rest.strip_prefix(' ')?.trim();
    (!title.is_empty()).then(|| (level, title.to_string()))
}

/// Heading text -> slug: alphanumerics and `_` survive (so `script_check` and
/// `allow_exact` stay typeable as themselves), every other run of characters
/// collapses to a single `-`.
fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut pending_sep = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_sep = true;
        }
    }
    out
}

/// Resolve `query` against `sections`, or explain why it didn't resolve.
///
/// Three passes, each narrowing the same way: exact slug, then slugs starting
/// with the query, then slugs containing it. Within a pass, several matches
/// collapse to a single answer when exactly one of them is a `##` - a parent
/// section contains its children's text, so answering with the parent is a
/// superset of what was asked for, never a different subject. `clippy`
/// therefore reaches the clippy phase rather than erroring between it and its
/// `allow` / `allow_exact` subsections.
pub(crate) fn find<'a>(sections: &'a [Section], query: &str) -> Result<&'a Section, String> {
    let want = slugify(query);
    if want.is_empty() {
        return Err("empty section name".to_string());
    }

    for matches in [
        pick(sections, |s| s.slug == want),
        pick(sections, |s| s.slug.starts_with(&want)),
        pick(sections, |s| s.slug.contains(&want)),
    ] {
        match matches.as_slice() {
            [] => continue,
            [only] => return Ok(only),
            several => {
                let tops: Vec<&Section> = several.iter().copied().filter(|s| s.level == 2).collect();
                if let [only] = tops.as_slice() {
                    return Ok(only);
                }
                let names: Vec<&str> = several.iter().map(|s| s.slug.as_str()).collect();
                return Err(format!("'{query}' is ambiguous: {}", names.join(", ")));
            }
        }
    }

    Err(format!("no section matching '{query}'"))
}

fn pick(sections: &[Section], f: impl Fn(&Section) -> bool) -> Vec<&Section> {
    sections.iter().filter(|s| f(s)).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{find, sections, slugify};

    const DOC: &str = "\
# Title

intro

## `[[script_check]]` array

body

### `stage`

more

## `clippy` phase

```sh
## not a heading
```

### `[clippy] allow`

### `[clippy] allow_exact`

## Per-sweep log lines (collapsed by default)

tail
";

    #[test]
    fn slugs_keep_underscores_and_collapse_the_rest() {
        assert_eq!(slugify("`[[script_check]]` array"), "script_check-array");
        assert_eq!(slugify("`[clippy] allow_exact`"), "clippy-allow_exact");
        assert_eq!(slugify("`certifies` and the exit-code contract"), "certifies-and-the-exit-code-contract");
    }

    #[test]
    fn fenced_hashes_are_not_headings() {
        let secs = sections(DOC);
        let slugs: Vec<&str> = secs.iter().map(|s| s.slug.as_str()).collect();
        assert_eq!(
            slugs,
            [
                "script_check-array",
                "stage",
                "clippy-phase",
                "clippy-allow",
                "clippy-allow_exact",
                "per-sweep-log-lines-collapsed-by-default",
            ]
        );
    }

    /// A `##` section runs to the next `##`, swallowing its `###` children -
    /// asking for `clippy` must not hand back a body that stops before
    /// `allow_exact`.
    #[test]
    fn a_parent_section_contains_its_children() {
        let secs = sections(DOC);
        let clippy = find(&secs, "clippy").unwrap();
        let body = &DOC[clippy.start..clippy.end];
        assert!(body.contains("allow_exact"), "{body}");
        assert!(!body.contains("Per-sweep"), "{body}");
    }

    /// Nobody types `script_check-array`; a prefix of the interesting half is
    /// what a reader actually knows.
    #[test]
    fn prefix_and_substring_both_resolve() {
        let secs = sections(DOC);
        assert_eq!(find(&secs, "script_check").unwrap().slug, "script_check-array");
        assert_eq!(find(&secs, "log lines").unwrap().slug, "per-sweep-log-lines-collapsed-by-default");
    }

    /// Two `###` siblings under one `##` have no parent to collapse to, so the
    /// user gets told what the candidates are instead of a coin flip.
    #[test]
    fn ambiguity_between_siblings_is_reported() {
        let secs = sections(DOC);
        let err = find(&secs, "allow").unwrap_err();
        assert!(err.contains("clippy-allow"), "{err}");
        assert!(err.contains("clippy-allow_exact"), "{err}");
    }

    #[test]
    fn an_unknown_section_says_so() {
        let secs = sections(DOC);
        assert!(find(&secs, "nonesuch").unwrap_err().contains("no section"));
    }
}
