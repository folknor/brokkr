//! The fixture-lint phase: prove clippy's `disallowed_*` lints actually fire.
//!
//! Those lints fail open. A configured path that no longer resolves (an item
//! moved, a typo) seals nothing, and a nested `clippy.toml` silently shadows
//! the root one. Nothing in the product build can notice. The only proof is
//! to lint a fixture crate that calls the sealed items and require the
//! diagnostics to appear - so this phase lints an in-tree fixture, once per
//! configured environment variant, and holds the result to a manifest.
//!
//! WHAT A GREEN RUN CLAIMS, per variant: the fixture's selected target was
//! analysed to completion (a successful, freshly compiled clippy run - its
//! artifact present with `fresh: false`), every manifest file was compiled
//! unconditionally (the strict module walk), every expected diagnostic
//! appeared in its file naming its item, and nothing else in a fixture file
//! fired. Expected diagnostics are therefore WARNINGS: rustc emits no
//! artifact after an error, so a deny-level run cannot prove completion.
//! Whether a lint is denied in the product is the product clippy phase's
//! question, not this one.
//!
//! FRESHNESS. Cargo replays a fresh unit's cached diagnostics, so variant two
//! could silently report variant one's output if clippy did not fingerprint
//! the variable that differs. Every variant therefore cleans the fixture
//! package first (in the same environment and target dir) and requires the
//! artifact to be non-fresh - an execution contract, not a toolchain
//! assumption.
//!
//! `--no-deps`: only the fixture is clippy-analysed. Product crates it
//! depends on are path dependencies, which would otherwise be linted too -
//! under the variant's `CLIPPY_CONF_DIR`, or their own config discovery -
//! and could fail the run before the fixture is reached.

mod lock;
mod manifest;
mod walk;

use std::path::{Path, PathBuf};

use crate::config::FixtureLint;
use crate::error::DevError;
use crate::output;

use super::{Phase, cargo_line, fmt_wall, phase_elapsed};

/// Run every selected `[[fixture_lint]]` entry; each failure is reported as
/// it is found and the phase fails once at the end.
pub(super) fn run_fixture_lint_phase(
    project_root: &Path,
    fixtures: &[FixtureLint],
    commands: bool,
) -> Result<(), DevError> {
    if fixtures.is_empty() {
        output::detail("fixture-lint: not applicable (no [[fixture_lint]] entry runs under this claim)");
        return Ok(());
    }
    let main_target = crate::build::project_info(Some(project_root))?.target_dir;
    let mut ok = true;
    let mut summaries = Vec::new();
    for f in fixtures {
        match lint_fixture(project_root, &main_target, f, commands)? {
            Some(summary) => summaries.push(summary),
            None => ok = false,
        }
    }
    if !ok {
        return Err(DevError::Reported("fixture-lint failed".into()));
    }
    output::run_msg(&format!(
        "fixture-lint: ok ({}) in {}",
        summaries.join("; "),
        fmt_wall(phase_elapsed())
    ));
    Ok(())
}

/// The fixture's one package and the production target the lint selects.
struct Fixture {
    dir: PathBuf,
    root_id: crate::cargo::metadata::PackageId,
    root_name: String,
    /// `--lib` or `--bin NAME`.
    selector: Vec<String>,
    /// The target's name and root source file.
    target: (String, PathBuf),
    /// Whether the selected target is the lib (artifact class check).
    is_lib: bool,
}

fn fail(name: &str, msg: &str) {
    output::error(&format!("fixture-lint {name}: {msg}"));
}

/// Lint one fixture under every variant. `Ok(Some(summary))` on a pass,
/// `Ok(None)` on reported findings, `Err` on an operational failure.
fn lint_fixture(
    project_root: &Path,
    main_target: &Path,
    f: &FixtureLint,
    commands: bool,
) -> Result<Option<String>, DevError> {
    output::status(&format!("fixture-lint {}", f.name));
    let dir = project_root.join(&f.path);
    let target_dir = main_target.join(format!("fixture-{}", f.name));
    let target_env = target_dir.to_string_lossy().into_owned();
    let env: Vec<(&str, &str)> = vec![("CARGO_TARGET_DIR", target_env.as_str())];

    if let Err(msg) = preflight(project_root, &dir, f) {
        fail(&f.name, &msg);
        return Ok(None);
    }

    // The lock transaction: seed, let cargo add the fixture's own entries,
    // validate, then every lint runs `--locked` and the lock must not move.
    let main_meta = crate::cargo::metadata::load(
        project_root,
        crate::cargo::metadata::Query { locked: true, ..Default::default() },
        &[],
        Some(Phase::FixtureLint),
    )?;
    let main_lock_path = Path::new(&main_meta.workspace_root).join("Cargo.lock");
    let fixture_lock_path = dir.join("Cargo.lock");
    if f.seed_lock {
        let Ok(main_lock) = std::fs::read(&main_lock_path) else {
            fail(&f.name, &format!("seed_lock: no main lock at {}", main_lock_path.display()));
            return Ok(None);
        };
        crate::atomic_write::replace(&fixture_lock_path, &main_lock)?;
        // Unlocked, once: cargo adds the fixture root and its path entries
        // and prunes what the fixture does not use.
        let args = crate::cargo::metadata::Query::resolving().cargo_args();
        cargo_line(commands, &format!("cargo {}", args.join(" ")));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let prep = output::run_cargo(&arg_refs, &dir, &env, Some(Phase::FixtureLint))?;
        if !prep.success() {
            fail(&f.name, &format!(
                "resolving the seeded lock failed:\n{}",
                String::from_utf8_lossy(&prep.stderr).trim_end()
            ));
            return Ok(None);
        }
    }
    let fixture_meta = crate::cargo::metadata::load(
        &dir,
        crate::cargo::metadata::Query { locked: true, ..Default::default() },
        &env,
        Some(Phase::FixtureLint),
    )?;
    let fixture = match shape(&dir, &fixture_meta) {
        Ok(fx) => fx,
        Err(msg) => {
            fail(&f.name, &msg);
            return Ok(None);
        }
    };

    let mut findings: Vec<String> = Vec::new();
    if f.seed_lock {
        let main_lock = std::fs::read_to_string(&main_lock_path)?;
        let fixture_lock = std::fs::read_to_string(&fixture_lock_path)?;
        match lock::containment_failures(&main_lock, &fixture_lock) {
            Ok(drift) => findings.extend(drift),
            Err(e) => findings.push(e),
        }
    }
    findings.extend(lock::path_failures(&main_meta, &fixture_meta, &fixture.root_id));

    let manifest = match std::fs::read_to_string(project_root.join(&f.manifest))
        .map_err(|e| e.to_string())
        .and_then(|t| manifest::Manifest::parse(&t))
    {
        Ok(m) => m,
        Err(e) => {
            fail(&f.name, &format!("manifest {}: {e}", f.manifest.display()));
            return Ok(None);
        }
    };
    findings.extend(inventory(&fixture, &manifest));
    if !findings.is_empty() {
        for finding in &findings {
            fail(&f.name, finding);
        }
        return Ok(None);
    }
    let lock_before = std::fs::read(&fixture_lock_path).ok();

    let mut ok = true;
    for v in &f.variants {
        ok &= lint_variant(project_root, f, v, &fixture, &manifest, &env, commands)?;
    }
    if std::fs::read(&fixture_lock_path).ok() != lock_before {
        fail(&f.name, "the fixture's Cargo.lock changed during the locked lint runs");
        ok = false;
    }
    if !ok {
        return Ok(None);
    }
    let expectations: usize = manifest.files.values().map(Vec::len).sum();
    Ok(Some(format!(
        "{}: {}, {}, {}, target {}",
        f.name,
        output::count(f.variants.len(), "variant"),
        output::count(manifest.files.len(), "file"),
        output::count(expectations, "expectation"),
        output::format_bytes(dir_size(&target_dir))
    )))
}

/// Checks that need only the filesystem and git.
fn preflight(project_root: &Path, dir: &Path, f: &FixtureLint) -> Result<(), String> {
    // In-tree means in-tree after `..` and symlinks: with `seed_lock` the
    // phase rewrites the fixture's lock, and must never do that elsewhere.
    let root = project_root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", project_root.display()))?;
    for (key, p) in [("path", dir.to_path_buf()), ("manifest", project_root.join(&f.manifest))] {
        let canonical = p.canonicalize().map_err(|e| format!("`{key}` {}: {e}", p.display()))?;
        if !canonical.starts_with(&root) || (key == "path" && canonical == root) {
            return Err(format!(
                "`{key}` {} resolves to {}, outside the project tree (or the root itself)",
                p.display(),
                canonical.display()
            ));
        }
    }
    let manifest_path = dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let toml: toml::Table = toml::from_str(&text).map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    if !toml.contains_key("workspace") || !toml.contains_key("package") {
        return Err(format!(
            "{} must be its own single-package workspace: a [package] with an empty [workspace] table",
            manifest_path.display()
        ));
    }
    if f.seed_lock {
        // The seeded lock is a build product; committed, it would dirty the
        // tree on every run.
        let lock = f.path.join("Cargo.lock");
        let lock_str = lock.to_string_lossy();
        let ignored = output::run_captured("git", &["check-ignore", "-q", lock_str.as_ref()], project_root)
            .map_err(|e| format!("git check-ignore: {e}"))?;
        if !ignored.success() {
            return Err(format!(
                "seed_lock rewrites {} on every run, so it must be gitignored - add it to .gitignore",
                lock.display()
            ));
        }
    }
    Ok(())
}

/// The fixture's package and selected target from its own resolve.
fn shape(dir: &Path, meta: &crate::cargo::metadata::CargoMetadata) -> Result<Fixture, String> {
    let [root_id] = meta.workspace_members.as_slice() else {
        return Err(format!(
            "the fixture workspace has {} members; it must be exactly one package",
            meta.workspace_members.len()
        ));
    };
    let pkg = meta
        .packages
        .iter()
        .find(|p| &p.id == root_id)
        .ok_or("the fixture's root package is missing from its metadata")?;
    let lib_kinds = ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];
    let lib = pkg
        .targets
        .iter()
        .find(|t| t.kind.iter().any(|k| lib_kinds.contains(&k.as_str())));
    let bins: Vec<_> = pkg.targets.iter().filter(|t| t.kind.iter().any(|k| k == "bin")).collect();
    let (selector, target, is_lib) = match (lib, bins.as_slice()) {
        (Some(t), _) => (vec!["--lib".to_owned()], t, true),
        (None, [t]) => (vec!["--bin".to_owned(), t.name.clone()], *t, false),
        (None, _) => {
            return Err(format!(
                "the fixture needs a lib target or exactly one bin; it has {} bins and no lib",
                bins.len()
            ));
        }
    };
    Ok(Fixture {
        dir: dir.to_path_buf(),
        root_id: root_id.clone(),
        root_name: pkg.name.clone(),
        selector,
        target: (target.name.clone(), PathBuf::from(&target.src_path)),
        is_lib,
    })
}

/// Fixture-relative, forward-slash spelling of a path under the fixture dir.
fn relative(dir: &Path, path: &Path) -> Option<String> {
    let dir = dir.canonicalize().ok()?;
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let rel = path.strip_prefix(&dir).ok()?;
    Some(rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
}

/// Every `.rs` file under the fixture dir, except its own `target/`.
fn on_disk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if !(d == dir && e.file_name() == "target") {
                    stack.push(p);
                }
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    out
}

/// Two-sided inventory: the files on disk equal the files listed, and every
/// listed file is reached unconditionally from the target root.
fn inventory(fixture: &Fixture, manifest: &manifest::Manifest) -> Vec<String> {
    let mut findings = Vec::new();
    let disk: std::collections::BTreeSet<String> =
        on_disk(&fixture.dir).iter().filter_map(|p| relative(&fixture.dir, p)).collect();
    for f in disk.iter().filter(|f| !manifest.files.contains_key(*f)) {
        findings.push(format!("{f} is on disk but not in the manifest"));
    }
    for f in manifest.files.keys().filter(|f| !disk.contains(*f)) {
        findings.push(format!("{f} is in the manifest but not on disk"));
    }
    let w = walk::walk(&fixture.target.1);
    findings.extend(w.refusals.iter().map(|r| format!("module walk: {r}")));
    let reached: std::collections::BTreeSet<String> =
        w.reached.iter().filter_map(|p| relative(&fixture.dir, p)).collect();
    for f in manifest.files.keys().filter(|f| disk.contains(*f) && !reached.contains(*f)) {
        findings.push(format!(
            "{f} is not reached from {} through unconditional `mod` declarations - it may never compile",
            fixture.target.0
        ));
    }
    findings
}

/// Lint one variant. `Ok(true)` on a pass; findings are reported here.
fn lint_variant(
    project_root: &Path,
    f: &FixtureLint,
    v: &crate::config::FixtureVariant,
    fixture: &Fixture,
    manifest: &manifest::Manifest,
    base_env: &[(&str, &str)],
    commands: bool,
) -> Result<bool, DevError> {
    let label = format!("{}/{}", f.name, v.name);
    output::status(&format!("fixture-lint {label}"));
    let root = project_root.to_string_lossy();
    let values: Vec<(String, String)> =
        v.env.iter().map(|(k, val)| (k.clone(), val.replace("{root}", &root))).collect();
    let mut env: Vec<(&str, &str)> = base_env.to_vec();
    env.extend(values.iter().map(|(k, val)| (k.as_str(), val.as_str())));
    let unset: Vec<&str> = v.unset.iter().map(String::as_str).collect();
    let run = |args: &[String]| -> Result<output::CapturedOutput, DevError> {
        cargo_line(commands, &format!("cargo {}", args.join(" ")));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        output::run_cargo_unsetting(&refs, &fixture.dir, &env, &unset, Some(Phase::FixtureLint))
    };

    let clean = run(&["clean".into(), "-p".into(), fixture.root_name.clone()])?;
    if !clean.success() {
        fail(&label, &format!(
            "cargo clean -p {} failed, so freshness cannot be guaranteed:\n{}",
            fixture.root_name,
            String::from_utf8_lossy(&clean.stderr).trim_end()
        ));
        return Ok(false);
    }

    let mut args: Vec<String> = vec!["clippy".into(), "--no-deps".into(), "--locked".into()];
    args.extend(fixture.selector.iter().cloned());
    args.push("--message-format=json".into());
    args.push("--".into());
    args.extend(f.lint_args.iter().cloned());
    let captured = run(&args)?;
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let diagnostics = crate::cargo_json::parse_cargo_diagnostics(&stdout);

    let mut failures: Vec<String> = Vec::new();
    for d in diagnostics.iter().filter(|d| d.is_error()) {
        failures.push(format!("error: {}", d.format_one()));
    }
    if !captured.success() {
        if failures.is_empty() {
            failures.push(format!(
                "cargo clippy failed:\n{}",
                String::from_utf8_lossy(&captured.stderr).trim_end()
            ));
        }
    } else if !fresh_artifact(&stdout, fixture) {
        failures.push(format!(
            "no freshly compiled artifact for {} in the clippy stream - the variant did not analyse the fixture",
            fixture.target.0
        ));
    }

    let root_id = fixture.root_id.to_string();
    let observed: Vec<manifest::Observed> = diagnostics
        .iter()
        .filter(|d| !d.is_error() && d.package_id.as_deref() == Some(root_id.as_str()))
        .map(|d| {
            let file = d.site.as_ref().and_then(|s| {
                let p = if s.file.is_relative() { fixture.dir.join(&s.file) } else { s.file.clone() };
                relative(&fixture.dir, &p).filter(|r| manifest.files.contains_key(r))
            });
            let at = d.site.as_ref().map_or_else(
                || "no span".to_owned(),
                |s| format!("{}:{}", s.file.display(), s.line.unwrap_or(0)),
            );
            manifest::Observed {
                file,
                lint: d.code.as_ref().map(|c| c.as_str().to_owned()),
                message: d.message.clone(),
                at,
            }
        })
        .collect();
    failures.extend(manifest::verdict(manifest, &observed));
    for failure in &failures {
        fail(&label, failure);
    }
    Ok(failures.is_empty())
}

/// The selected target's artifact, compiled (not replayed) and not as a test.
fn fresh_artifact(stdout: &str, fixture: &Fixture) -> bool {
    #[derive(serde::Deserialize)]
    struct Artifact {
        reason: String,
        package_id: String,
        target: Target,
        #[serde(default)]
        fresh: bool,
        #[serde(default)]
        profile: Option<Profile>,
    }
    #[derive(serde::Deserialize)]
    struct Target {
        name: String,
        kind: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct Profile {
        #[serde(default)]
        test: bool,
    }
    let root = fixture.root_id.to_string();
    stdout.lines().filter_map(|l| serde_json::from_str::<Artifact>(l).ok()).any(|a| {
        a.reason == "compiler-artifact"
            && a.package_id == root
            && a.target.name == fixture.target.0
            && a.target.kind.iter().any(|k| (k == "bin") != fixture.is_lib)
            && !a.fresh
            && !a.profile.is_some_and(|p| p.test)
    })
}

/// Bytes under a directory, best-effort (a report figure).
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(e.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn fixture(is_lib: bool) -> Fixture {
        Fixture {
            dir: PathBuf::new(),
            root_id: "path+file:///f#seal@0.0.0".into(),
            root_name: "seal".into(),
            selector: Vec::new(),
            target: ("seal".into(), PathBuf::new()),
            is_lib,
        }
    }

    // FRESHNESS IS AN EXECUTION CONTRACT: a replayed (fresh) artifact, or a
    // test-harness compile, does not prove this variant analysed anything.
    #[test]
    fn only_a_non_fresh_non_test_artifact_of_the_target_counts() {
        let line = |fresh: bool, test: bool, kind: &str| {
            format!(
                r#"{{"reason":"compiler-artifact","package_id":"path+file:///f#seal@0.0.0","target":{{"name":"seal","kind":["{kind}"]}},"fresh":{fresh},"profile":{{"test":{test}}}}}"#
            )
        };
        assert!(fresh_artifact(&line(false, false, "lib"), &fixture(true)));
        assert!(!fresh_artifact(&line(true, false, "lib"), &fixture(true)));
        assert!(!fresh_artifact(&line(false, true, "lib"), &fixture(true)));
        // A lib artifact does not stand in for a selected bin.
        assert!(!fresh_artifact(&line(false, false, "lib"), &fixture(false)));
        assert!(fresh_artifact(&line(false, false, "bin"), &fixture(false)));
    }
}
