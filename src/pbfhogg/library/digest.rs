//! `pbfhogg digest FILE`'s report, the equality CLI equality reads
//! (`docs/commands/library.md`, CLI equality): exactly four lines,
//! `xxh128 v<N> <hex>`, then `node`, `way` and `relation`, each
//! `<kind> <count> <hex>`, the hex 32 lowercase digits, every line ending in
//! a newline, nothing else. brokkr implements canonical versions 1 and 2 of
//! the `xxh128` algorithm, which share that grammar and differ in what the
//! hashes cover (v2 adds metadata, `visible`, header fields and
//! `LocationsOnWays` coordinates); any other is refused naming it, never
//! compared, since a new canonical version means a new content definition.

/// The algorithm brokkr compares, and its canonical versions.
const ALGORITHM: &str = "xxh128";
const VERSIONS: [&str; 2] = ["v1", "v2"];

/// The kinds of the per-kind lines, in report order.
pub(crate) const KINDS: [&str; 3] = ["node", "way", "relation"];

/// A parsed report: its canonical version, the whole-content hash and each
/// kind's count and hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    /// `v1` or `v2`.
    pub(crate) version: String,
    pub(crate) all: String,
    pub(crate) kinds: [(u64, String); 3],
}

/// Read a report, refusing anything but the four-line `xxh128 v1` or `v2`
/// shape.
pub(crate) fn parse(stdout: &[u8]) -> Result<Report, String> {
    let text = std::str::from_utf8(stdout).map_err(|_| "the report is not UTF-8".to_owned())?;
    let Some(body) = text.strip_suffix('\n') else {
        return Err("the report does not end in a newline".to_owned());
    };
    let lines: Vec<&str> = body.split('\n').collect();
    let [first, node, way, relation] = lines.as_slice() else {
        return Err(format!("the report has {} lines, not 4", lines.len()));
    };
    let head: Vec<&str> = first.split(' ').collect();
    let [algorithm, version, all] = head.as_slice() else {
        return Err(format!("line 1 {first:?} is not `<algorithm> v<N> <hash>`"));
    };
    if *algorithm != ALGORITHM || !VERSIONS.contains(version) {
        return Err(format!(
            "the report is {algorithm} {version}; brokkr implements {ALGORITHM} {} only",
            VERSIONS.join(" and ")
        ));
    }
    let all =
        hash(all).ok_or_else(|| format!("line 1 {first:?} has no 32-digit lowercase hash"))?;
    let mut kinds: [(u64, String); 3] = Default::default();
    for (i, (line, kind)) in [node, way, relation].into_iter().zip(KINDS).enumerate() {
        let parts: Vec<&str> = line.split(' ').collect();
        let parsed = match parts.as_slice() {
            [name, count, digest] if *name == kind => count_of(count).zip(hash(digest)),
            _ => None,
        };
        kinds[i] = parsed
            .ok_or_else(|| format!("line {} {line:?} is not `{kind} <count> <hash>`", i + 2))?;
    }
    Ok(Report {
        version: (*version).to_owned(),
        all,
        kinds,
    })
}

fn hash(text: &str) -> Option<String> {
    (text.len() == 32
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then(|| text.to_owned())
}

/// A decimal count without sign or leading zero.
fn count_of(text: &str) -> Option<u64> {
    let canonical = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if canonical { text.parse().ok() } else { None }
}

/// Each way `library` and `cli` differ, one line each; empty when equal.
/// Reports of two canonical versions differ by that alone: one CLI digests
/// both outputs, so a mismatch is a defect, never compared across versions.
pub(crate) fn differences(library: &Report, cli: &Report) -> Vec<String> {
    if library.version != cli.version {
        return vec![format!(
            "canonical version: library {ALGORITHM} {}, CLI {ALGORITHM} {}, which one CLI never \
             reports for two outputs",
            library.version, cli.version
        )];
    }
    let mut out = Vec::new();
    if library.all != cli.all {
        out.push(format!(
            "content hash: library {}, CLI {}",
            library.all, cli.all
        ));
    }
    for ((kind, (lc, lh)), (cc, ch)) in KINDS.iter().zip(&library.kinds).zip(&cli.kinds) {
        if lc != cc || lh != ch {
            out.push(format!("{kind}: library {lc} {lh}, CLI {cc} {ch}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const H: &str = "0123456789abcdef0123456789abcdef";

    fn report(first: &str) -> String {
        format!("{first}\nnode 12 {H}\nway 3 {H}\nrelation 0 {H}\n")
    }

    #[test]
    fn the_four_line_report_parses_in_either_version() {
        let r = parse(report(&format!("xxh128 v1 {H}")).as_bytes()).unwrap();
        assert_eq!(r.version, "v1");
        assert_eq!(r.all, H);
        assert_eq!(r.kinds[0], (12, H.to_owned()));
        assert_eq!(r.kinds[2].0, 0);
        let r = parse(report(&format!("xxh128 v2 {H}")).as_bytes()).unwrap();
        assert_eq!(r.version, "v2");
    }

    #[test]
    fn another_algorithm_or_version_is_refused_by_name() {
        let e = parse(report(&format!("xxh128 v3 {H}")).as_bytes()).unwrap_err();
        assert!(
            e.contains("xxh128 v3; brokkr implements xxh128 v1 and v2 only"),
            "{e}"
        );
        let e = parse(report(&format!("sha256 v1 {H}")).as_bytes()).unwrap_err();
        assert!(e.contains("sha256 v1"), "{e}");
    }

    #[test]
    fn reports_of_two_versions_are_a_defect_whatever_their_hashes() {
        let v1 = parse(report(&format!("xxh128 v1 {H}")).as_bytes()).unwrap();
        let v2 = parse(report(&format!("xxh128 v2 {H}")).as_bytes()).unwrap();
        let d = differences(&v1, &v2);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].contains("library xxh128 v1, CLI xxh128 v2"), "{d:?}");
    }

    #[test]
    fn anything_but_the_exact_shape_is_refused() {
        let good = report(&format!("xxh128 v1 {H}"));
        for bad in [
            good.trim_end().to_owned(),
            format!("{good}extra\n"),
            good.replacen("node 12", "node 012", 1),
            good.replacen("way 3", "way -3", 1),
            good.replacen("relation", "rel", 1),
            good.replacen(&format!("v1 {H}"), &format!("v1 {}", H.to_uppercase()), 1),
            good.replacen("node 12 ", "node 12  ", 1),
            good.replacen("node", "way", 1),
        ] {
            assert!(parse(bad.as_bytes()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn differences_name_the_hash_and_each_kind() {
        let a = parse(report(&format!("xxh128 v1 {H}")).as_bytes()).unwrap();
        assert!(differences(&a, &a).is_empty());
        let mut b = a.clone();
        b.all = "f".repeat(32);
        b.kinds[1].0 = 4;
        let d = differences(&a, &b);
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(d[0].starts_with("content hash"));
        assert!(d[1].starts_with("way: library 3"));
    }
}
