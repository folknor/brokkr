//! `brokkr deps <pkg>` - metadata + chain trace for one package.
//!
//! Given a spec like `"indexmap"` or `"hashbrown@0.17.1"`, prints a
//! one-line metadata header (`source=...  manifest=...`) and every
//! distinct chain from a workspace member down to the target. Uses
//! host-filtered metadata by default so we don't surface chains that
//! only exist for inactive targets. If the spec doesn't resolve in
//! the host-filtered graph, falls back to the unfiltered graph and
//! says so. If it doesn't resolve there either, falls back to a
//! substring search across all package names so `brokkr deps mime`
//! enumerates `mime`, `mime_guess`, ... instead of erroring out -
//! matches what people kept hand-rolling against `cargo metadata`.

use std::collections::BTreeSet;
use std::path::Path;

use super::{
    CargoMetadata, CargoPackage, PackageSourceKind, load_metadata, load_metadata_host_filtered,
};
use crate::error::DevError;
use crate::output;

pub(super) fn run_focus(project_root: &Path, spec: &str, json: bool) -> Result<(), DevError> {
    let (name, version) = parse_spec(spec);
    let host_md = load_metadata_host_filtered(project_root)?;

    if has_exact_match(&host_md, name, version) {
        return emit_traces(&host_md, &exact_predicate(name, version), None, json);
    }

    let unfiltered = load_metadata(project_root)?;
    if has_exact_match(&unfiltered, name, version) {
        let note = format!("{spec}: not in host-filtered graph; showing all-target chains");
        return emit_traces(
            &unfiltered,
            &exact_predicate(name, version),
            Some(&note),
            json,
        );
    }

    // Substring fallback: case-insensitive `contains` on package name.
    // Honours the optional `@version` suffix if the user supplied one.
    let needle = name.to_ascii_lowercase();
    let predicate: Box<dyn Fn(&CargoPackage) -> bool> = Box::new(move |p| {
        p.name.to_ascii_lowercase().contains(&needle) && version.is_none_or(|v| p.version == v)
    });
    let match_count = unfiltered.packages.iter().filter(|p| predicate(p)).count();
    if match_count == 0 {
        return Err(DevError::Refused(format!(
            "no package matching {spec:?} in this workspace's resolve graph"
        )));
    }
    let note = format!(
        "no exact match for {spec:?}; showing {match_count} substring match{}",
        if match_count == 1 { "" } else { "es" }
    );
    emit_traces(&unfiltered, &predicate, Some(&note), json)
}

fn emit_traces(
    md: &CargoMetadata,
    predicate: &dyn Fn(&CargoPackage) -> bool,
    prefix_note: Option<&str>,
    json: bool,
) -> Result<(), DevError> {
    let mut targets: Vec<&CargoPackage> = md.packages.iter().filter(|p| predicate(p)).collect();
    targets.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));

    let traces: Vec<ChainTrace> = targets
        .iter()
        .map(|p| ChainTrace {
            krate: p.name.clone(),
            version: p.version.clone(),
            source: format_source(p, md.source_kind(&p.id)),
            manifest_path: tildify(&p.manifest_path),
            prefix_note: prefix_note.map(str::to_owned),
            chains: chains_to_workspace(md, &p.id),
        })
        .collect();

    if json {
        for t in &traces {
            let line = serde_json::to_string(t).map_err(|error| {
                DevError::Internal(format!(
                    "could not serialize dependency focus result: {error}"
                ))
            })?;
            println!("{line}");
        }
        return Ok(());
    }

    if let Some(note) = prefix_note {
        output::deps_msg(note);
    }
    for t in &traces {
        let chain_label = if t.chains.len() == 1 {
            "1 chain".to_string()
        } else {
            format!("{} chains", t.chains.len())
        };
        output::deps_msg(&format!(
            "{} {}  source={}  manifest={}  ({chain_label})",
            t.krate, t.version, t.source, t.manifest_path
        ));
        if t.chains.is_empty() {
            output::deps_msg("  (no parents - package is a workspace member or unreachable)");
            continue;
        }
        for chain in &t.chains {
            output::deps_msg(&format!("  {}", chain.join(" -> ")));
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
pub struct ChainTrace {
    #[serde(rename = "crate")]
    pub krate: String,
    pub version: String,
    /// Normalised source label: `crates.io`, `git+<url>#<sha>`,
    /// `registry=<url>` (git or sparse index), `workspace`, or `path`. Replaces the raw
    /// `source` string from cargo metadata which is verbose and full
    /// of `registry+` URL noise.
    pub source: String,
    /// Manifest path with `$HOME` prefix replaced by `~` for read-
    /// ability. The JSON value is the same shortened form so scripts
    /// match what the text renderer shows.
    pub manifest_path: String,
    /// Why focus mode fell back from an exact host-graph match, when it did.
    /// Included in JSON so consumers can distinguish an exact result from a
    /// result selected by an all-target or substring fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix_note: Option<String>,
    /// Chains ordered workspace-root first, target last.
    pub chains: Vec<Vec<String>>,
}

fn parse_spec(spec: &str) -> (&str, Option<&str>) {
    match spec.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (spec, None),
    }
}

fn has_exact_match(md: &CargoMetadata, name: &str, version: Option<&str>) -> bool {
    md.packages
        .iter()
        .any(|p| p.name == name && version.is_none_or(|v| p.version == v))
}

fn exact_predicate<'a>(
    name: &'a str,
    version: Option<&'a str>,
) -> impl Fn(&CargoPackage) -> bool + 'a {
    move |p: &CargoPackage| p.name == name && version.is_none_or(|v| p.version == v)
}

/// Normalised source label. Cargo spells a registry source two ways:
/// `registry+<url>` for a git-protocol index and `sparse+<url>` for a
/// sparse one (crates.io itself is canonicalised to its git-index id, but
/// the sparse form is accepted too in case that ever changes). Both become
/// `registry=<url>` - the protocol is transport, not provenance.
fn format_source(p: &CargoPackage, source_kind: PackageSourceKind) -> String {
    const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";
    const CRATES_IO_SPARSE: &str = "sparse+https://index.crates.io/";
    match source_kind {
        PackageSourceKind::Member => "workspace".to_string(),
        PackageSourceKind::Path => "path".to_string(),
        PackageSourceKind::Git => p.source.clone().unwrap_or_else(|| "git".to_string()),
        PackageSourceKind::Registry => {
            let Some(s) = p.source.as_deref() else {
                return "path".to_string();
            };
            if s == CRATES_IO || s == CRATES_IO_SPARSE {
                return "crates.io".to_string();
            }
            match s
                .strip_prefix("registry+")
                .or_else(|| s.strip_prefix("sparse+"))
            {
                Some(rest) => format!("registry={rest}"),
                None => s.to_string(),
            }
        }
    }
}

fn tildify(path: &str) -> String {
    tildify_with_home(path, crate::user_dirs::home().as_deref())
}

fn tildify_with_home(path: &str, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(rest) = Path::new(path).strip_prefix(home)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return Path::new("~").join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}

/// BFS upward from `target_id`. Each branch ends when it reaches a
/// workspace member. Cycle prevention only applies to non-workspace
/// nodes so distinct paths can converge on the same workspace root.
fn chains_to_workspace(metadata: &CargoMetadata, target_id: &super::PackageId) -> Vec<Vec<String>> {
    metadata
        .normal_paths_to_members(target_id, false)
        .into_iter()
        .map(|path| path.iter().rev().map(|id| metadata.label(id)).collect())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn pkg(source: Option<&str>) -> CargoPackage {
        CargoPackage {
            name: "x".to_string(),
            version: "1.0.0".to_string(),
            id: "x-id".into(),
            source: source.map(str::to_string),
            manifest_path: String::new(),
            links: None,
            publish: None,
            dependencies: Vec::new(),
            targets: Vec::new(),
            features: Default::default(),
            authors: Vec::new(),
            description: None,
            homepage: None,
            license: None,
            license_file: None,
            repository: None,
            rust_version: None,
        }
    }

    #[test]
    fn format_source_normalises_both_registry_protocols() {
        let label = |s| format_source(&pkg(Some(s)), PackageSourceKind::Registry);
        assert_eq!(
            label("registry+https://github.com/rust-lang/crates.io-index"),
            "crates.io"
        );
        assert_eq!(label("sparse+https://index.crates.io/"), "crates.io");
        assert_eq!(
            label("registry+https://example.com/index"),
            "registry=https://example.com/index"
        );
        assert_eq!(
            label("sparse+https://example.com/index/"),
            "registry=https://example.com/index/"
        );
        assert_eq!(
            format_source(
                &pkg(Some("git+https://example.com/r#abc")),
                PackageSourceKind::Git,
            ),
            "git+https://example.com/r#abc"
        );
        assert_eq!(format_source(&pkg(None), PackageSourceKind::Path), "path");
        assert_eq!(
            format_source(&pkg(None), PackageSourceKind::Member),
            "workspace"
        );
    }

    #[test]
    fn focus_json_keeps_the_fallback_note() {
        let trace = ChainTrace {
            krate: "x".into(),
            version: "1.0.0".into(),
            source: "crates.io".into(),
            manifest_path: "~/Cargo.toml".into(),
            prefix_note: Some("x: not in host-filtered graph; showing all-target chains".into()),
            chains: Vec::new(),
        };
        let value = serde_json::to_value(trace).unwrap();
        assert_eq!(
            value["prefix_note"],
            "x: not in host-filtered graph; showing all-target chains"
        );
    }

    #[test]
    fn exact_focus_json_omits_an_absent_note() {
        let trace = ChainTrace {
            krate: "x".into(),
            version: "1.0.0".into(),
            source: "crates.io".into(),
            manifest_path: "~/Cargo.toml".into(),
            prefix_note: None,
            chains: Vec::new(),
        };
        let value = serde_json::to_value(trace).unwrap();
        assert!(value.get("prefix_note").is_none());
    }

    #[test]
    fn tildify_uses_the_supplied_home() {
        assert_eq!(
            tildify_with_home(
                "/home/folk/.cargo/registry/src",
                Some(Path::new("/home/folk"))
            ),
            "~/.cargo/registry/src"
        );
        assert_eq!(
            tildify_with_home("/opt/cargo/registry/src", Some(Path::new("/home/folk"))),
            "/opt/cargo/registry/src"
        );
        assert_eq!(
            tildify_with_home("relative/Cargo.toml", None),
            "relative/Cargo.toml"
        );
    }
}
