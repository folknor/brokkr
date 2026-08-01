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

    // A subsection is addressed through its parent unless it already names it.
    // `### stage` under `## [[script_check]]` is a fine heading to read *in
    // place* and a useless thing to type at a shell - `script_check-stage`
    // says which stage. `### [clippy] allow` already leads with its parent, so
    // it is left alone rather than stuttering into `clippy-clippy-allow`.
    let mut parent = String::new();
    heads
        .into_iter()
        .zip(ends)
        .map(|((level, title, start), end)| {
            let slug = slugify(&title);
            let slug = if level == 2 {
                parent.clone_from(&slug);
                slug
            } else if parent.is_empty() || slug.starts_with(parent.as_str()) {
                slug
            } else {
                format!("{parent}-{slug}")
            };
            Section { level, slug, start, end }
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

/// Words that name a heading's *shape* rather than its subject. A trailing one
/// is dropped: the section on `[[script_check]]` is addressed as
/// `script_check`, not `script_check-array`, because nobody looking it up is
/// thinking about whether the TOML spelling is an array or a table.
const SHAPE_WORDS: &[&str] = &["section", "array", "entries", "entry", "blocks", "block", "phase"];

/// Heading text -> slug: alphanumerics and `_` survive (so `script_check` and
/// `allow_exact` stay typeable as themselves), every other run of characters
/// collapses to a single `-`.
///
/// Two subtractions keep the result to the subject itself: a parenthesised
/// tail is dropped (it qualifies the heading for a reader, but `coverage` is
/// the name of the thing, not `coverage-complete-profiles`), and so is a
/// trailing [`SHAPE_WORDS`] token.
fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut pending_sep = false;
    let mut depth = 0u32;
    for ch in title.chars() {
        match ch {
            '(' => {
                depth += 1;
                pending_sep = true;
            }
            ')' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ if ch.is_ascii_alphanumeric() || ch == '_' => {
                if pending_sep && !out.is_empty() {
                    out.push('-');
                }
                pending_sep = false;
                out.push(ch.to_ascii_lowercase());
            }
            _ => pending_sep = true,
        }
    }

    for word in SHAPE_WORDS {
        if let Some(head) = out.strip_suffix(word)
            && let Some(head) = head.strip_suffix('-')
        {
            return head.to_string();
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

/// Put several resolved sections into document order and drop any whose text
/// another already carries.
///
/// Asking for a `##` and one of its `###` children is a normal thing to type
/// (`brokkr man check clippy allow_exact`) and printing the child twice, once
/// inside its parent, would read as a rendering bug. Document order rather
/// than argv order, because the reader is getting a slice of one document and
/// the doc's own sequencing is the one that makes sense of it.
pub(crate) fn merge(mut hits: Vec<&Section>) -> Vec<&Section> {
    // Widest-first at a shared start, so a container is always seen before
    // anything it contains.
    hits.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
    let mut out: Vec<&Section> = Vec::with_capacity(hits.len());
    for hit in hits {
        if !matches!(out.last(), Some(prev) if hit.end <= prev.end) {
            out.push(hit);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{find, merge, sections, slugify};

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
        assert_eq!(slugify("`[[script_check]]` array"), "script_check");
        assert_eq!(slugify("`[clippy] allow_exact`"), "clippy-allow_exact");
        assert_eq!(slugify("`certifies` and exit codes"), "certifies-and-exit-codes");
    }

    /// The shape word is the last token or it is part of the subject - a
    /// `[[dependency_rule]]` array is addressed as `dependency_rule`, but a
    /// heading whose subject *is* the word keeps it.
    #[test]
    fn shape_words_and_parenthesised_tails_drop() {
        assert_eq!(slugify("`[gremlins]` section"), "gremlins");
        assert_eq!(slugify("`coverage` phase (complete profiles)"), "coverage");
        assert_eq!(slugify("Per-sweep log lines (collapsed by default)"), "per-sweep-log-lines");
        assert_eq!(slugify("Doctests"), "doctests");
        assert_eq!(slugify("`stage`"), "stage");
    }

    #[test]
    fn fenced_hashes_are_not_headings() {
        let secs = sections(DOC);
        let slugs: Vec<&str> = secs.iter().map(|s| s.slug.as_str()).collect();
        assert_eq!(
            slugs,
            [
                "script_check",
                "script_check-stage",
                "clippy",
                "clippy-allow",
                "clippy-allow_exact",
                "per-sweep-log-lines",
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
        assert_eq!(find(&secs, "script").unwrap().slug, "script_check");
        assert_eq!(find(&secs, "log lines").unwrap().slug, "per-sweep-log-lines");
    }

    /// The point of qualifying a subsection: `stage` is what the heading says,
    /// and both spellings must reach it.
    #[test]
    fn a_subsection_is_reachable_through_its_parent_or_its_own_name() {
        let secs = sections(DOC);
        assert_eq!(find(&secs, "script_check-stage").unwrap().slug, "script_check-stage");
        assert_eq!(find(&secs, "stage").unwrap().slug, "script_check-stage");
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

    /// Several sections come back in document order regardless of the order
    /// they were asked for.
    #[test]
    fn merge_orders_by_document_position() {
        let secs = sections(DOC);
        let hits = vec![
            find(&secs, "log lines").unwrap(),
            find(&secs, "script_check").unwrap(),
        ];
        let slugs: Vec<&str> = merge(hits).iter().map(|s| s.slug.as_str()).collect();
        assert_eq!(slugs, ["script_check", "per-sweep-log-lines"]);
    }

    /// A child whose parent is also selected is already on screen; printing it
    /// again would read as a bug.
    #[test]
    fn merge_drops_a_section_its_neighbour_contains() {
        let secs = sections(DOC);
        let hits = vec![
            find(&secs, "allow_exact").unwrap(),
            find(&secs, "clippy").unwrap(),
            find(&secs, "allow_exact").unwrap(),
        ];
        let slugs: Vec<&str> = merge(hits).iter().map(|s| s.slug.as_str()).collect();
        assert_eq!(slugs, ["clippy"]);
    }

    #[test]
    fn an_unknown_section_says_so() {
        let secs = sections(DOC);
        assert!(find(&secs, "nonesuch").unwrap_err().contains("no section"));
    }
}
