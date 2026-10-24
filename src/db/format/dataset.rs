//! Dataset-aware short naming for the results table and compare view.
//!
//! Maps a stored `input_file` to a short dataset label. The label is what
//! shows in the `dataset` column of `brokkr results`. When configured
//! dataset keys are available (loaded from `brokkr.toml`), the matcher
//! prefers the longest configured key that is a prefix of the basename
//! so datasets with hyphens in their names (`greater-london`,
//! `north-america`) survive intact. With no configured keys - or a
//! filename that doesn't match any of them - it falls back to the first
//! dash-separated component of the basename.
//!
//! A content-addressed input (`PATH@<32 hex xxh128>`, what `brokkr dellingr`
//! records for a workload) is labelled by its path part plus a digest prefix,
//! so two generations of one file never render alike. The prefix is the
//! shortest one - at least [`MIN_GENERATION_HEX`] characters - that tells
//! apart every generation of that path among the rows being rendered, which
//! is why the renderers hand the matcher their rows ([`DatasetMatcher::for_inputs`]).

use std::collections::BTreeMap;
use std::path::Path;

/// The shortest digest prefix a generation label shows.
const MIN_GENERATION_HEX: usize = 8;

/// Short-label resolver for `input_file` values.
///
/// Construct with [`DatasetMatcher::new`] from the list of configured
/// dataset keys, or `DatasetMatcher::empty` (test-only) when no config is
/// available (tests, ad-hoc callers). Keys are stored sorted
/// longest-first so [`short_name`](Self::short_name) does a simple
/// linear scan and returns on the first prefix match.
#[derive(Clone)]
pub struct DatasetMatcher {
    keys: Vec<String>,
    /// Every content generation among the rows being rendered, by path part.
    generations: BTreeMap<String, Vec<String>>,
}

/// Split a content-addressed input into its path and its full digest. Only a
/// trailing `@` followed by exactly 32 lowercase hex characters counts.
pub fn split_generation(input_file: &str) -> (&str, Option<&str>) {
    match input_file.rsplit_once('@') {
        Some((path, digest))
            if digest.len() == 32
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
        {
            (path, Some(digest))
        }
        _ => (input_file, None),
    }
}

impl DatasetMatcher {
    pub fn new<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut keys: Vec<String> = keys.into_iter().map(Into::into).collect();
        keys.sort();
        keys.dedup();
        // Longest first so "greater-london" wins over "greater" if both
        // ever appear as configured dataset keys.
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
        Self {
            keys,
            generations: BTreeMap::new(),
        }
    }

    #[cfg(test)]
    pub fn empty() -> Self {
        Self::new(Vec::<String>::new())
    }

    /// This matcher, aware of every content generation among `inputs` - the
    /// rows about to be rendered - so each generation label is unambiguous
    /// within that set.
    pub fn for_inputs<'a>(&self, inputs: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out = self.clone();
        for input in inputs {
            if let (path, Some(digest)) = split_generation(input) {
                let seen = out.generations.entry(path.to_owned()).or_default();
                if !seen.iter().any(|d| d == digest) {
                    seen.push(digest.to_owned());
                }
            }
        }
        out
    }

    /// Return a short dataset label for the given `input_file`.
    pub fn short_name(&self, input_file: &str) -> String {
        if input_file.is_empty() {
            return String::new();
        }
        let (path, digest) = split_generation(input_file);
        let label = self.path_label(path);
        match digest {
            Some(digest) => format!("{label}@{}", &digest[..self.prefix_len(path, digest)]),
            None => label,
        }
    }

    /// The shortest prefix of `digest`, at least [`MIN_GENERATION_HEX`] long,
    /// that no other known generation of `path` shares.
    fn prefix_len(&self, path: &str, digest: &str) -> usize {
        let others: Vec<&String> = self
            .generations
            .get(path)
            .map(|g| g.iter().filter(|d| d.as_str() != digest).collect())
            .unwrap_or_default();
        (MIN_GENERATION_HEX..digest.len())
            .find(|&n| others.iter().all(|o| !o.starts_with(&digest[..n])))
            .unwrap_or(digest.len())
    }

    fn path_label(&self, input_file: &str) -> String {
        let basename = Path::new(input_file)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(input_file);
        for key in &self.keys {
            if is_prefix_match(basename, key) {
                return key.clone();
            }
        }
        // Fallback: use file_stem + first dash-separated component.
        let stem = Path::new(basename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(basename);
        stem.split_once('-')
            .map_or(stem, |(head, _)| head)
            .to_owned()
    }
}

/// True if `basename` starts with `key` followed by a `-`, a `.`, or
/// end-of-string (so `denmark` matches `denmark-...osm.pbf` and
/// `denmark.pbf` but not `denmarkish-x.pbf`).
fn is_prefix_match(basename: &str, key: &str) -> bool {
    if !basename.starts_with(key) {
        return false;
    }
    match basename.as_bytes().get(key.len()) {
        None => true,
        Some(&b) => b == b'-' || b == b'.',
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::*;

    fn matcher() -> DatasetMatcher {
        DatasetMatcher::new([
            "denmark",
            "europe",
            "greater-london",
            "north-america",
            "planet",
        ])
    }

    #[test]
    fn hyphenated_dataset_wins_over_first_dash() {
        let m = matcher();
        assert_eq!(
            m.short_name("greater-london-20260225-seq4704-with-indexdata.osm.pbf"),
            "greater-london"
        );
        assert_eq!(
            m.short_name("north-america-seq4710-with-indexdata.osm.pbf"),
            "north-america"
        );
    }

    #[test]
    fn simple_dataset_resolves_from_config() {
        let m = matcher();
        assert_eq!(
            m.short_name("europe-20260301-seq4714-with-indexdata.osm.pbf"),
            "europe"
        );
        assert_eq!(m.short_name("denmark-raw.osm.pbf"), "denmark");
    }

    #[test]
    fn unknown_filename_falls_back_to_first_dash() {
        let m = matcher();
        assert_eq!(
            m.short_name("switzerland-20260225-seq4707.osm.pbf"),
            "switzerland"
        );
    }

    #[test]
    fn empty_and_pathy_inputs() {
        let m = matcher();
        assert_eq!(m.short_name(""), "");
        assert_eq!(
            m.short_name("data/inputs/europe-20260301.osm.pbf"),
            "europe"
        );
        assert_eq!(m.short_name("rawfile"), "rawfile");
    }

    #[test]
    fn empty_matcher_preserves_legacy_heuristic() {
        let m = DatasetMatcher::empty();
        assert_eq!(
            m.short_name("greater-london-20260225-seq4704.osm.pbf"),
            "greater"
        );
        // Legacy heuristic splits on '-' only; stems without a dash come
        // through with their sub-extensions intact.
        assert_eq!(m.short_name("denmark.osm.pbf"), "denmark.osm");
    }

    #[test]
    fn a_content_generation_keeps_its_digest_in_the_label() {
        let m = DatasetMatcher::empty();
        let digest = "51b15378d6e75699f24b1fd2d025b30a";
        assert_eq!(
            m.short_name(&format!("bench/method.lua@{digest}")),
            "method@51b15378"
        );
        // Not 32 lowercase hex: an ordinary path that happens to hold an `@`.
        assert_eq!(m.short_name("data/x@abc.osm.pbf"), "x@abc.osm");
    }

    #[test]
    fn generations_sharing_a_prefix_are_told_apart() {
        let a = "bench/method.lua@12345678aaaaaaaaaaaaaaaaaaaaaaaa";
        let b = "bench/method.lua@12345678abbbbbbbbbbbbbbbbbbbbbbb";
        let other = "bench/sort.lua@12345678cccccccccccccccccccccccc";
        let m = DatasetMatcher::empty().for_inputs([a, b, other, a]);
        assert_eq!(m.short_name(a), "method@12345678aa");
        assert_eq!(m.short_name(b), "method@12345678ab");
        // Another path's generation does not lengthen this one.
        assert_eq!(m.short_name(other), "sort@12345678");
    }

    #[test]
    fn prefix_that_isnt_a_boundary_does_not_match() {
        let m = DatasetMatcher::new(["den"]);
        // "denmark..." should NOT be matched as "den".
        assert_eq!(m.short_name("denmark-raw.osm.pbf"), "denmark");
    }
}
