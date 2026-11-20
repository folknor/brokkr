//! The build stamp `brokkr install` writes into every binary it installs, and
//! the bin names `brokkr install --names` prints, brokkr#205.
//!
//! A host ran a days-old brokkr for days and nothing noticed. The stamp makes
//! the installed file say which commit it came from, so a fleet tool can read
//! it and compare with the checkout's HEAD without running the binary. It is
//! one line, `sha <full commit id> clean <y|n>`, in an ELF section of its own
//! that nothing loads: brokkr adds it after cargo installs the file, so no
//! build input or fingerprint changes and no project carries any code for it.
//!
//! What it claims: the checkout at the package's git top level was at that
//! commit, clean or not by [`TreeState::for_install`], when install sampled it
//! under the lock. It does not see edits made while brokkr runs, changes git
//! is told to hide (assume-unchanged, skip-worktree), ignored build inputs,
//! path dependencies outside the checkout, or cargo's freshness judgment.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::cargo::metadata::CargoPackage;
use crate::error::DevError;
use crate::git::TreeState;

/// The ELF section holding the stamp.
pub(crate) const SECTION: &str = ".brokkr.build_info";

/// The stamp for one package's checkout, sampled once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stamp {
    /// The section's bytes: the line and a newline.
    pub(crate) content: String,
}

/// Sample the checkout holding `package_dir`: its HEAD and whether it is
/// clean. `None` outside a git checkout or before its first commit, where
/// there is no commit to name.
pub(crate) fn sample(package_dir: &Path) -> Result<Option<Stamp>, DevError> {
    let Some(toplevel) = git_line(package_dir, &["rev-parse", "--show-toplevel"]) else {
        return Ok(None);
    };
    let toplevel = PathBuf::from(toplevel);
    let Some(sha) = git_line(&toplevel, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
    else {
        return Ok(None);
    };
    if sha.is_empty() || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DevError::Build(format!(
            "git named HEAD of {} as {sha:?}, not a commit id",
            toplevel.display()
        )));
    }
    let state = TreeState::read_for_install(&toplevel).ok_or_else(|| {
        DevError::Build(format!(
            "cannot read git status of {} for the build stamp",
            toplevel.display()
        ))
    })?;
    let clean = if state.for_install() { "n" } else { "y" };
    Ok(Some(Stamp {
        content: format!("sha {sha} clean {clean}\n"),
    }))
}

fn git_line(dir: &Path, args: &[&str]) -> Option<String> {
    let out = crate::output::child::command_output(
        crate::process::spawn::ChildPolicy::plain_command("git")
            .args(args)
            .current_dir(dir),
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8(out.stdout).ok()?;
    Some(line.trim().to_owned())
}

/// Where `cargo install --path` puts a package's bins when nothing overrides
/// the root, or why brokkr cannot be sure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallRoot {
    /// The cargo home: brokkr passes it as `--root`, so cargo and the stamp
    /// agree on the directory by construction, and cargo appends `bin`.
    Default(PathBuf),
    /// `CARGO_INSTALL_ROOT` or an `install.root` in a config cargo reads:
    /// install leaves cargo to choose and stamps nothing.
    Overridden(String),
}

/// Resolve the install root for one package from the environment cargo will
/// get (`env`) and every cargo config it can read: the chain from the package
/// directory, where `install --path` reloads config, and from the build root,
/// cargo's cwd. Reading both is conservative: an override in either keeps
/// brokkr out of the choice.
pub(crate) fn install_root(
    build_root: &Path,
    package_dir: &Path,
    env: crate::rustflags::EffectiveEnv<'_>,
) -> Result<InstallRoot, DevError> {
    if env
        .get_os("CARGO_INSTALL_ROOT")
        .is_some_and(|value| !value.is_empty())
    {
        return Ok(InstallRoot::Overridden("CARGO_INSTALL_ROOT is set".into()));
    }
    for dir in [package_dir, build_root] {
        for (path, doc) in crate::rustflags::read_config_files_under(dir, env)? {
            let root = doc
                .get("install")
                .and_then(toml::Value::as_table)
                .is_some_and(|install| install.contains_key("root"));
            if root {
                return Ok(InstallRoot::Overridden(format!(
                    "install.root is set in {}",
                    path.display()
                )));
            }
        }
    }
    // Cargo treats an empty CARGO_HOME as unset and resolves a relative one
    // against its cwd, the build root.
    let home = match env.get_os("CARGO_HOME").filter(|value| !value.is_empty()) {
        Some(value) => build_root.join(PathBuf::from(value)),
        None => crate::user_dirs::home()
            .map(|home| home.join(".cargo"))
            .ok_or_else(|| DevError::Config("no home directory for the cargo home".into()))?,
    };
    Ok(InstallRoot::Default(home))
}

/// The bin names `cargo install --path` installs for `package`: every bin
/// target whose `required-features` are all enabled by the package's default
/// features, since install passes no feature flags.
///
/// Exact where it answers, refusing where it cannot be: a requirement on a
/// dependency's feature (`dep/feat`) depends on the resolved graph, and an
/// own-feature requirement whose answer hinges on whether a `dep/feat` entry
/// also enables an optional dependency's implicit feature (which the
/// metadata cannot tell from an explicit one) names the bin and refuses.
pub(crate) fn installable_bins(package: &CargoPackage) -> Result<Vec<String>, DevError> {
    let lower = default_closure(&package.features, false);
    let upper = default_closure(&package.features, true);
    let mut out = Vec::new();
    for target in &package.targets {
        if !target.kind.iter().any(|kind| kind == "bin") {
            continue;
        }
        if let Some(req) = target.required_features.iter().find(|f| f.contains('/')) {
            return Err(DevError::Refused(format!(
                "bin '{}' of {} requires '{req}', a dependency feature: whether cargo \
                 install builds it depends on the resolved graph, so brokkr cannot name it",
                target.name, package.name
            )));
        }
        let enabled = |set: &HashSet<String>| {
            target
                .required_features
                .iter()
                .all(|feature| set.contains(feature))
        };
        match (enabled(&lower), enabled(&upper)) {
            (true, true) => out.push(target.name.clone()),
            (false, false) => {}
            _ => {
                return Err(DevError::Refused(format!(
                    "bin '{}' of {}: whether its required-features are on by default \
                     depends on how cargo reads a dep/feat entry, which the metadata \
                     cannot settle",
                    target.name, package.name
                )));
            }
        }
    }
    Ok(out)
}

/// The own features `default` enables, following only own-feature edges.
/// `dep:name` enables no own feature and `dep?/feat` none either; a plain
/// `dep/feat` enables the own feature `dep` only when `implicit` is set, the
/// reading where that name is an optional dependency's implicit feature.
fn default_closure(features: &HashMap<String, Vec<String>>, implicit: bool) -> HashSet<String> {
    let mut on = HashSet::new();
    let mut todo: Vec<&str> = Vec::new();
    if features.contains_key("default") {
        todo.push("default");
    }
    while let Some(feature) = todo.pop() {
        if !on.insert(feature.to_owned()) {
            continue;
        }
        for value in features.get(feature).into_iter().flatten() {
            if value.starts_with("dep:") {
                continue;
            }
            let own = match value.split_once('/') {
                Some((dep, _)) if dep.ends_with('?') => continue,
                Some((dep, _)) if implicit => dep,
                Some(_) => continue,
                None => value.as_str(),
            };
            if features.contains_key(own) {
                todo.push(own);
            }
        }
    }
    on
}

/// Write `stamp` into the installed file at `path` as [`SECTION`]: through a
/// copy beside it that `objcopy` writes, checked to hold exactly the stamp,
/// given the file's permissions and renamed over it, so the path always
/// names either cargo's file or the stamped one.
pub(crate) fn stamp_file(path: &Path, stamp: &Stamp) -> Result<(), DevError> {
    let dir = path
        .parent()
        .ok_or_else(|| DevError::Build(format!("{} has no parent", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| DevError::Build(format!("{} has no file name", path.display())))?
        .to_string_lossy()
        .into_owned();
    let scratch = Scratch::new(dir, &name);
    if !stamp_sections(path)?.is_empty() {
        return Err(DevError::Build(format!(
            "{} already carries {SECTION}; cargo installed a file brokkr did not expect",
            path.display()
        )));
    }
    std::fs::write(&scratch.content, stamp.content.as_bytes())?;
    let mut section = std::ffi::OsString::from(format!("{SECTION}="));
    section.push(scratch.content.as_os_str());
    let out = crate::output::child::command_output(
        crate::process::spawn::ChildPolicy::plain_command("objcopy")
            .arg("--add-section")
            .arg(&section)
            .arg(path)
            .arg(&scratch.stamped),
    )
    .map_err(|e| DevError::Build(format!("cannot run objcopy to stamp {}: {e}", path.display())))?;
    if !out.status.success() {
        return Err(DevError::Build(format!(
            "objcopy could not stamp {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    match stamp_sections(&scratch.stamped)?.as_slice() {
        [bytes] if bytes.as_slice() == stamp.content.as_bytes() => {}
        _ => {
            return Err(DevError::Build(format!(
                "the stamped copy of {} does not read back its {SECTION}",
                path.display()
            )));
        }
    }
    let permissions = std::fs::metadata(path)?.permissions();
    std::fs::set_permissions(&scratch.stamped, permissions)?;
    std::fs::rename(&scratch.stamped, path)?;
    Ok(())
}

/// Every [`SECTION`] in `file`, read natively (`crate::elf_section`).
fn stamp_sections(file: &Path) -> Result<Vec<Vec<u8>>, DevError> {
    crate::elf_section::read_sections(file, SECTION).map_err(DevError::Build)
}

/// The scratch files one stamp uses beside the installed file, removed
/// however the stamp ends.
struct Scratch {
    content: PathBuf,
    stamped: PathBuf,
}

impl Scratch {
    fn new(dir: &Path, name: &str) -> Self {
        let base = format!(".brokkr-stamp-{}-{name}", std::process::id());
        Self {
            content: dir.join(format!("{base}.info")),
            stamped: dir.join(&base),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in [&self.content, &self.stamped] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => crate::output::warn(&format!(
                    "could not remove stamp scratch {}: {e}",
                    path.display()
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;

    use super::{default_closure, installable_bins};

    fn package(json: serde_json::Value) -> crate::cargo::metadata::CargoPackage {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn installable_bins_follow_required_features() {
        let p = package(serde_json::json!({
            "name": "app", "id": "app",
            "features": { "default": ["fast"], "fast": [], "slow": [] },
            "targets": [
                { "name": "app", "kind": ["bin"] },
                { "name": "quick", "kind": ["bin"], "required-features": ["fast"] },
                { "name": "lazy", "kind": ["bin"], "required-features": ["slow"] },
                { "name": "applib", "kind": ["lib"] }
            ]
        }));
        assert_eq!(installable_bins(&p).unwrap(), ["app", "quick"]);
    }

    #[test]
    fn a_dependency_feature_requirement_refuses() {
        let p = package(serde_json::json!({
            "name": "app", "id": "app",
            "targets": [{ "name": "x", "kind": ["bin"], "required-features": ["engine/fast"] }]
        }));
        assert!(installable_bins(&p).is_err());
    }

    #[test]
    fn an_implicit_feature_ambiguity_refuses() {
        let p = package(serde_json::json!({
            "name": "app", "id": "app",
            "features": { "default": ["x/feat"], "x": ["dep:x"] },
            "targets": [{ "name": "b", "kind": ["bin"], "required-features": ["x"] }]
        }));
        assert!(installable_bins(&p).is_err());
    }

    fn features(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.iter().map(|s| (*s).to_owned()).collect()))
            .collect()
    }

    #[test]
    fn closure_follows_own_edges_from_default() {
        let f = features(&[
            ("default", &["a"]),
            ("a", &["b", "dep:x", "y?/z"]),
            ("b", &[]),
            ("c", &[]),
        ]);
        let on = default_closure(&f, false);
        assert!(on.contains("a") && on.contains("b"));
        assert!(!on.contains("c"));
    }

    #[test]
    fn no_default_enables_nothing() {
        let f = features(&[("a", &[])]);
        assert!(default_closure(&f, false).is_empty());
    }

    #[test]
    fn a_dep_feat_entry_differs_only_in_the_implicit_reading() {
        let f = features(&[("default", &["x/feat"]), ("x", &["dep:x"])]);
        assert!(!default_closure(&f, false).contains("x"));
        assert!(default_closure(&f, true).contains("x"));
    }
}
