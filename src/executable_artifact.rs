//! Picking the executable a `cargo build --message-format=json` produced.
//!
//! The build names what it wants with `-p SPEC`, `--bin NAME` or
//! `--example NAME`, and the same selection picks the record: package by
//! the record's full `package_id` (name, version, source, as cargo's package
//! spec grammar reads them, globs included), target by `target.kind` and
//! `target.name`. A file stem is never the key: a bin and an example share
//! one, and so does every copy of one target. Test-profile units are
//! skipped.
//!
//! Exactly one record may survive. Several do when one target is compiled
//! more than once in a build - one per compile target when cargo config sets
//! several `build.target`s (brokkr passes no `--target`), or an
//! artifact-dependency unit beside the requested one - and the lookup
//! refuses rather than pick by cargo's unordered output. What it cannot
//! tell apart is a requested unit from a dependency artifact when only the
//! latter was built (nightly artifact dependencies with `required-features`
//! off for the root): cargo's records do not say which units were roots, so
//! that case is cargo's to expose, not something a selector here can close.

use std::path::PathBuf;

use crate::error::DevError;

/// What the build asked for, as it was passed to cargo.
pub(crate) struct Selector<'a> {
    pub(crate) package: Option<&'a str>,
    pub(crate) bin: Option<&'a str>,
    pub(crate) example: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub(crate) struct CargoExecutableArtifact {
    pub(crate) path: PathBuf,
    /// The record's `target.name`.
    pub(crate) target: String,
    /// The record's `profile` object, as [`crate::build_provenance::unit_pairs`].
    pub(crate) unit_profile: Vec<(String, String)>,
    /// The record's `features`: what cargo resolved for this unit.
    pub(crate) features: Vec<String>,
}

struct Record {
    package_id: String,
    kinds: Vec<String>,
    artifact: CargoExecutableArtifact,
}

/// The one executable in cargo's JSON `stdout` that `selector` names.
pub(crate) fn find(
    stdout: &[u8],
    selector: &Selector<'_>,
) -> Result<CargoExecutableArtifact, DevError> {
    let records = parse(stdout)?;
    let package = selector.package.map(PackageSpec::parse).transpose()?;
    let in_package = |r: &&Record| {
        package
            .as_ref()
            .is_none_or(|spec| spec.matches(&r.package_id))
    };
    let named = |kind: &str, name: &str| -> Result<Vec<&Record>, DevError> {
        let pattern = NamePattern::new(name)?;
        Ok(records
            .iter()
            .filter(in_package)
            .filter(|r| r.kinds.iter().any(|k| k == kind) && pattern.matches(&r.artifact.target))
            .collect())
    };
    let (candidates, what) = if let Some(bin) = selector.bin {
        (named("bin", bin)?, format!("bin `{bin}`"))
    } else if let Some(example) = selector.example {
        (named("example", example)?, format!("example `{example}`"))
    } else if let Some(spec) = &package {
        let bins: Vec<&Record> = records
            .iter()
            .filter(in_package)
            .filter(|r| r.kinds.iter().any(|k| k == "bin"))
            .collect();
        let mut names: Vec<&str> = bins.iter().map(|r| r.artifact.target.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        // Cargo's `-p` alone builds every bin of the package.
        let chosen = spec
            .name
            .as_deref()
            .map_or_else(|| sole(&names), |pkg| package_bin(&names, pkg));
        match chosen {
            Some(name) => (
                bins.into_iter()
                    .filter(|r| r.artifact.target == name)
                    .collect(),
                format!("bin `{name}` of package `{}`", spec.raw),
            ),
            None if names.is_empty() => (Vec::new(), format!("a bin of package `{}`", spec.raw)),
            None => {
                return Err(DevError::Build(format!(
                    "package `{}` built {} bins ({}) and none is named like it; name one with --bin",
                    spec.raw,
                    names.len(),
                    names.join(", ")
                )));
            }
        }
    } else {
        (records.iter().collect(), "an executable".to_owned())
    };
    match candidates.as_slice() {
        [one] => Ok(one.artifact.clone()),
        [] if records.is_empty() => Err(DevError::Build("no executable in cargo output".into())),
        [] => Err(DevError::Build(format!(
            "cargo output has no executable for {what}; it reported {}",
            records
                .iter()
                .map(|r| format!("`{}`", r.artifact.target))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        many => Err(DevError::Build(format!(
            "cargo output has {} executables for {what}: {}. One target compiled several times \
             is ambiguous: several configured build targets (`build.target`, \
             `CARGO_BUILD_TARGET`) or an artifact dependency on it; build for a single target",
            many.len(),
            many.iter()
                .map(|r| crate::output::path(&r.artifact.path).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Which bin a selection naming only `package` means, given the package's
/// distinct bin names: its sole bin, else the one named like the package,
/// else none - an undecided choice the caller refuses, never settles by
/// order. Shared by this lookup and `build::resolve_existing_binary`, which
/// applies it to cargo metadata's declared bins instead of built records.
pub(crate) fn package_bin<'a>(names: &[&'a str], package: &str) -> Option<&'a str> {
    sole(names).or_else(|| names.iter().copied().find(|n| *n == package))
}

fn sole<'a>(names: &[&'a str]) -> Option<&'a str> {
    match names {
        [one] => Some(*one),
        _ => None,
    }
}

fn parse(stdout: &[u8]) -> Result<Vec<Record>, DevError> {
    let text = String::from_utf8_lossy(stdout);
    let mut records = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        let line_number = line_index + 1;
        let val =
            crate::cargo_json::parse_cargo_json_line(line, line_number).map_err(DevError::Build)?;
        let Some(val) = val else {
            continue;
        };
        let Some(reason) = val.get("reason").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if reason != "compiler-artifact" {
            if val
                .get("executable")
                .is_some_and(|executable| !executable.is_null())
            {
                return Err(DevError::Build(format!(
                    "unexpected cargo JSON record `{reason}` with an executable on stdout line {line_number}"
                )));
            }
            continue;
        }
        let malformed = |what: &str| {
            DevError::Build(format!(
                "malformed compiler-artifact on cargo stdout line {line_number}: {what}"
            ))
        };
        let Some(executable) = val.get("executable") else {
            return Err(malformed("missing `executable`"));
        };
        if executable.is_null() {
            continue;
        }
        let Some(exe) = executable.as_str() else {
            return Err(malformed("`executable` is not a string or null"));
        };
        if exe.is_empty() {
            return Err(malformed("empty `executable`"));
        }
        let profile = val.get("profile");
        if profile
            .and_then(|p| p.get("test"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            continue;
        }
        let target = val.get("target");
        let name = target
            .and_then(|t| t.get("name"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| malformed("missing `target.name`"))?;
        let kinds = target
            .and_then(|t| t.get("kind"))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| malformed("missing `target.kind`"))?
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect();
        let package_id = val
            .get("package_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| malformed("missing `package_id`"))?
            .to_owned();
        let features = val
            .get("features")
            .and_then(serde_json::Value::as_array)
            .map(|features| {
                features
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        records.push(Record {
            package_id,
            kinds,
            artifact: CargoExecutableArtifact {
                path: PathBuf::from(exe),
                target: name.to_owned(),
                unit_profile: profile
                    .map(crate::build_provenance::unit_pairs)
                    .unwrap_or_default(),
                features,
            },
        });
    }
    Ok(records)
}

/// A target or package name as cargo takes it: literal, or a glob when it
/// holds `*`, `?` or `[`.
enum NamePattern {
    Literal(String),
    Glob(globset::GlobMatcher),
}

impl NamePattern {
    fn new(raw: &str) -> Result<Self, DevError> {
        if raw.contains(['*', '?', '[']) {
            let glob = globset::Glob::new(raw)
                .map_err(|e| DevError::Build(format!("invalid name pattern `{raw}`: {e}")))?;
            Ok(Self::Glob(glob.compile_matcher()))
        } else {
            Ok(Self::Literal(raw.to_owned()))
        }
    }

    fn matches(&self, name: &str) -> bool {
        match self {
            Self::Literal(literal) => literal == name,
            Self::Glob(glob) => glob.is_match(name),
        }
    }
}

/// The parts of a cargo package id or spec: `name`, `name@ver`, `name:ver`,
/// `url`, `url#ver`, `url#name`, `url#name@ver`, and the legacy
/// `name ver (url)` id. A URL may carry a `kind+` source prefix.
#[derive(Debug, PartialEq, Eq)]
struct IdParts {
    source: Option<String>,
    name: Option<String>,
    version: Option<String>,
}

fn split_name_version(text: &str) -> (String, Option<String>) {
    match text.split_once(['@', ':']) {
        Some((name, version)) => (name.to_owned(), Some(version.to_owned())),
        None => (text.to_owned(), None),
    }
}

fn last_segment(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn id_parts(text: &str) -> IdParts {
    if let Some((name, rest)) = text.split_once(' ') {
        // Legacy `name ver (source)`.
        let mut rest = rest.splitn(2, ' ');
        let version = rest.next().map(str::to_owned);
        let source = rest
            .next()
            .map(|s| s.trim_start_matches('(').trim_end_matches(')').to_owned());
        return IdParts {
            source,
            name: Some(name.to_owned()),
            version,
        };
    }
    if !text.contains("://") {
        let (name, version) = split_name_version(text);
        return IdParts {
            source: None,
            name: Some(name),
            version,
        };
    }
    let (url, fragment) = match text.split_once('#') {
        Some((url, fragment)) => (url, Some(fragment)),
        None => (text, None),
    };
    let (name, version) = match fragment {
        None => (last_segment(url), None),
        Some(fragment) if fragment.starts_with(|c: char| c.is_ascii_digit()) => {
            (last_segment(url), Some(fragment.to_owned()))
        }
        Some(fragment) => {
            let (name, version) = split_name_version(fragment);
            (Some(name), version)
        }
    };
    IdParts {
        source: Some(url.to_owned()),
        name,
        version,
    }
}

/// A `-p` spec, matched against a record's `package_id`.
struct PackageSpec {
    raw: String,
    name: Option<String>,
    pattern: Option<NamePattern>,
    version: Option<String>,
    source: Option<String>,
}

impl PackageSpec {
    fn parse(raw: &str) -> Result<Self, DevError> {
        let parts = id_parts(raw);
        let pattern = parts.name.as_deref().map(NamePattern::new).transpose()?;
        Ok(Self {
            raw: raw.to_owned(),
            name: parts.name,
            pattern,
            version: parts.version,
            source: parts.source,
        })
    }

    fn matches(&self, package_id: &str) -> bool {
        let id = id_parts(package_id);
        let name_ok = match (&self.pattern, &id.name) {
            (None, _) => true,
            (Some(pattern), Some(name)) => pattern.matches(name),
            (Some(_), None) => false,
        };
        // A partial version (`1.2`) matches every version it prefixes.
        let version_ok = match (&self.version, &id.version) {
            (None, _) => true,
            (Some(want), Some(have)) => {
                have == want
                    || have
                        .strip_prefix(want.as_str())
                        .is_some_and(|r| r.starts_with('.'))
            }
            (Some(_), None) => false,
        };
        // A spec without a `kind+` prefix matches any source kind; one with
        // it must match the kind too.
        let source_ok = match (&self.source, &id.source) {
            (None, _) => true,
            (Some(want), Some(have)) => {
                let have = if want.contains('+') {
                    have.as_str()
                } else {
                    have.split_once('+').map_or(have.as_str(), |(_, url)| url)
                };
                have == want
                    || have
                        .strip_prefix(want.as_str())
                        .is_some_and(|r| r.starts_with('?'))
            }
            (Some(_), None) => false,
        };
        name_ok && version_ok && source_ok
    }
}

#[cfg(test)]
mod tests {
    use super::{Selector, find};

    fn record(id: &str, kind: &str, name: &str, path: &str, test: bool) -> String {
        format!(
            r#"{{"reason":"compiler-artifact","package_id":"{id}","target":{{"name":"{name}","kind":["{kind}"]}},"profile":{{"test":{test}}},"features":[],"executable":"{path}"}}"#
        )
    }

    fn stdout(records: &[String]) -> Vec<u8> {
        records.join("\n").into_bytes()
    }

    const APP: &str = "path+file:///w/app#0.1.0";
    const ENGINE2: &str = "path+file:///w/engine#engine@2.0.0";
    const ENGINE1: &str = "registry+https://github.com/rust-lang/crates.io-index#engine@1.0.0";

    fn sel<'a>(
        package: Option<&'a str>,
        bin: Option<&'a str>,
        example: Option<&'a str>,
    ) -> Selector<'a> {
        Selector {
            package,
            bin,
            example,
        }
    }

    #[test]
    fn one_target_built_for_two_compile_targets_is_refused() {
        let out = stdout(&[
            record(
                APP,
                "bin",
                "app",
                "/w/target/x86_64-unknown-linux-gnu/debug/app",
                false,
            ),
            record(
                APP,
                "bin",
                "app",
                "/w/target/aarch64-unknown-linux-gnu/debug/app",
                false,
            ),
        ]);
        let err = find(&out, &sel(Some("app"), Some("app"), None))
            .expect_err("refused")
            .to_string();
        assert!(err.contains("2 executables for bin `app`"), "{err}");
        assert!(err.contains("build.target"), "{err}");
        let err = find(&out, &sel(Some("app"), None, None))
            .expect_err("refused")
            .to_string();
        assert!(err.contains("2 executables"), "{err}");
    }

    #[test]
    fn bin_and_example_sharing_a_stem_resolve_by_kind() {
        let out = stdout(&[
            record(APP, "bin", "demo", "/w/target/debug/demo", false),
            record(
                APP,
                "example",
                "demo",
                "/w/target/debug/examples/demo",
                false,
            ),
        ]);
        let ex = find(&out, &sel(Some("app"), None, Some("demo"))).expect("selected");
        assert!(ex.path.ends_with("examples/demo"));
        let bin = find(&out, &sel(Some("app"), Some("demo"), None)).expect("selected");
        assert!(bin.path.ends_with("debug/demo"));
    }

    #[test]
    fn package_spec_qualifiers_exclude_other_versions() {
        // The requested package's sole bin is `worker`; a dependency of the
        // same name but another version supplies a bin named like the package.
        let out = stdout(&[
            record(ENGINE2, "bin", "worker", "/w/target/debug/worker", false),
            record(
                ENGINE1,
                "bin",
                "engine",
                "/w/target/debug/deps/engine",
                false,
            ),
        ]);
        let got = find(&out, &sel(Some("engine@2.0.0"), None, None)).expect("selected");
        assert_eq!(got.target, "worker");
        let got = find(&out, &sel(Some("engine:2"), None, None)).expect("selected");
        assert_eq!(got.target, "worker");
        let got = find(&out, &sel(Some("file:///w/engine#2.0"), None, None)).expect("selected");
        assert_eq!(got.target, "worker");
        assert!(find(&out, &sel(Some("registry+file:///w/engine"), None, None)).is_err());
    }

    #[test]
    fn package_only_prefers_the_bin_named_like_the_package_and_refuses_otherwise() {
        let out = stdout(&[
            record(APP, "bin", "helper", "/w/target/debug/helper", false),
            record(APP, "bin", "app", "/w/target/debug/app", false),
        ]);
        assert_eq!(
            find(&out, &sel(Some("app"), None, None))
                .expect("selected")
                .target,
            "app"
        );
        let out = stdout(&[
            record(APP, "bin", "helper", "/w/target/debug/helper", false),
            record(APP, "bin", "other", "/w/target/debug/other", false),
        ]);
        let err = find(&out, &sel(Some("app"), None, None))
            .expect_err("refused")
            .to_string();
        assert!(err.contains("none is named like it"), "{err}");
    }

    #[test]
    fn globs_filter_as_predicates_and_test_units_are_skipped() {
        let out = stdout(&[
            record(
                "path+file:///w/app-cli#0.1.0",
                "bin",
                "cli",
                "/w/target/debug/cli",
                false,
            ),
            record(
                "path+file:///w/tools#0.1.0",
                "bin",
                "helper",
                "/w/target/debug/helper",
                false,
            ),
            record(
                "path+file:///w/app-cli#0.1.0",
                "bin",
                "cli",
                "/w/target/debug/deps/cli-abc",
                true,
            ),
        ]);
        assert_eq!(
            find(&out, &sel(Some("app-*"), None, None))
                .expect("selected")
                .target,
            "cli"
        );
        assert_eq!(
            find(&out, &sel(None, Some("c*"), None))
                .expect("selected")
                .target,
            "cli"
        );
    }

    #[test]
    fn no_selector_needs_exactly_one_executable() {
        let one = stdout(&[record(APP, "bin", "app", "/w/target/release/app", false)]);
        assert_eq!(
            find(&one, &sel(None, None, None)).expect("selected").target,
            "app"
        );
        let two = stdout(&[
            record(APP, "bin", "app", "/w/target/release/app", false),
            record(APP, "bin", "b", "/w/target/release/b", false),
        ]);
        assert!(find(&two, &sel(None, None, None)).is_err());
        let err = find(b"", &sel(None, None, None))
            .expect_err("refused")
            .to_string();
        assert!(err.contains("no executable"), "{err}");
    }
}
