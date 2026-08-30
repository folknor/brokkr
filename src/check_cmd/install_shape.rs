// The install-feature phase: compile the `[bin] install` packages the way
// `cargo install` will resolve them.
//
// WHY THE WORKSPACE'S GREEN DOES NOT COVER THIS. Every other compiling phase
// resolves features over a selection that includes sibling workspace members,
// so the feature set of a shared dependency is the UNION of what all members
// ask for. `cargo install --path <pkg>` resolves that package alone. A call
// site in an install package that only compiles because a sibling enables a
// third-party feature (tokio's, hyper's, a nautilus crate's) is green under
// every workspace-selected sweep and fails at install time - on the deploy
// path, which is the worst place to learn about it.
//
// The phase runs `cargo check` over the install set under cargo's
// `resolver.feature-unification="package"` mode (RFC 3692), which resolves
// each selected package's features as though it were selected alone - the
// exact boundary `cargo install` uses. One invocation covers the whole set;
// the RFC defines multi-`-p` package mode as equivalent to N separate
// single-package builds for feature purposes.
//
// WHAT A GREEN RUN CLAIMS, PRECISELY: each install package's binary source
// compiles without features contributed solely by sibling workspace roots.
// It does NOT claim the packages will `cargo install`: the check shares the
// workspace lockfile (install re-resolves unless `--locked`), and `cargo
// check` performs no codegen or linking. The phase's own output says as much,
// so it cannot accrue trust it has not earned.
//
// THE ZERO-TARGET REFUSAL is not optional. `--bins` silently skips a bin
// whose `required-features` are unsatisfied, so a package could be "checked"
// with none of its binaries compiled - a gate-shaped no-op, the
// declared-but-never-evaluated failure this repo's consumers have shipped
// twice under other names. The artifact stream is therefore compared against
// the package's expected bin targets, and a missing bin fails the phase.
//
// Gate-only by default (`[bin] install_feature_check`): package mode
// deliberately compiles duplicate variants of shared dependencies, which is
// priced for the pre-landing run, not the edit loop.

/// Cargo args pinning package-mode feature resolution - the resolution
/// boundary `cargo install` uses. CLI `--config` outranks a repo's committed
/// `.cargo/config.toml` pin (e.g. a workspace-mode pin adopted for the
/// parallel test lane), which is what lets both coexist.
fn package_unification_args() -> [String; 3] {
    [
        "-Zfeature-unification".into(),
        "--config".into(),
        "resolver.feature-unification=\"package\"".into(),
    ]
}

/// Should the phase run under this mode and profile claim?
fn install_feature_applies(
    mode: Option<crate::config::InstallFeatureCheck>,
    certifies: Option<Certifies>,
) -> bool {
    use crate::config::InstallFeatureCheck as M;
    match mode.unwrap_or(M::Gate) {
        M::Off => false,
        M::Gate => certifies == Some(Certifies::Complete),
        M::Always => true,
    }
}

/// One expected-vs-checked comparison entry: a bin target the artifact
/// stream must contain for its package to be credited.
fn missing_bins(
    expected: &[(String, Vec<String>)],
    checked: &std::collections::BTreeSet<(String, String)>,
) -> Vec<String> {
    let mut missing = Vec::new();
    for (pkg, bins) in expected {
        for bin in bins {
            if !checked.contains(&(pkg.clone(), bin.clone())) {
                missing.push(format!("{pkg}/{bin}"));
            }
        }
    }
    missing
}

/// Parse cargo's artifact stream into the set of (package, bin target)
/// pairs the invocation actually checked.
fn checked_bins(stdout: &str) -> std::collections::BTreeSet<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Artifact {
        reason: String,
        package_id: String,
        target: ArtifactTarget,
    }
    #[derive(serde::Deserialize)]
    struct ArtifactTarget {
        name: String,
        kind: Vec<String>,
    }

    let mut out = std::collections::BTreeSet::new();
    for line in stdout.lines() {
        let Ok(a) = serde_json::from_str::<Artifact>(line) else {
            continue;
        };
        if a.reason != "compiler-artifact" || !a.target.kind.iter().any(|k| k == "bin") {
            continue;
        }
        out.insert((package_name_from_id(&a.package_id), a.target.name));
    }
    out
}

/// Best-effort attribution for a package-mode failure: which features does
/// the workspace resolve enable that the install-shaped resolve does not?
///
/// This is the difference between a confusing failure and an actionable one.
/// A package-mode error is an ordinary rustc error against a call site that
/// compiles fine under `brokkr check`'s other phases, so the first person to
/// hit one will reasonably assume the gate is broken. Naming the mechanism -
/// the sibling-contributed features the solo resolve loses - turns it into
/// "add the feature to this package's own manifest".
///
/// Best-effort in every direction: a `cargo tree` failure degrades to
/// nothing rather than masking the compile error it was meant to explain.
fn explain_lost_features(project_root: &Path, packages: &[String], env: &[(&str, &str)]) {
    let base = [
        "tree",
        "-e",
        "features",
        "--prefix",
        "none",
        "-f",
        "{p}\t{f}",
    ];
    let ws: Vec<&str> = base.iter().copied().chain(["--workspace"]).collect();
    let unify = package_unification_args();
    let mut scoped: Vec<&str> = base.to_vec();
    for p in packages {
        scoped.push("-p");
        scoped.push(p);
    }
    scoped.extend(unify.iter().map(String::as_str));

    let features_of = |args: &[&str]| -> Option<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>> {
        let captured = output::run_captured_with_env("cargo", args, project_root, env).ok()?;
        if !captured.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&captured.stdout);
        let mut map: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
            std::collections::BTreeMap::new();
        for line in stdout.lines() {
            let Some((pkg, feats)) = line.split_once('\t') else {
                continue;
            };
            // Cargo appends a dedupe marker to repeated subtrees; not a feature.
            let feats = feats.replace("(*)", "");
            map.entry(pkg.trim().to_owned())
                .or_default()
                .extend(feats.split(',').map(str::trim).filter(|f| !f.is_empty()).map(str::to_owned));
        }
        Some(map)
    };
    let (Some(ws_map), Some(scoped_map)) = (features_of(&ws), features_of(&scoped)) else {
        return;
    };

    let mut lost: Vec<String> = Vec::new();
    for (pkg, scoped_feats) in &scoped_map {
        let Some(ws_feats) = ws_map.get(pkg) else {
            continue;
        };
        let diff: Vec<&str> = ws_feats.difference(scoped_feats).map(String::as_str).collect();
        if !diff.is_empty() {
            // The version-suffixed package name, kept: two versions of one
            // dep can resolve differently and the reader needs to know which.
            lost.push(format!("{pkg}: {}", diff.join(", ")));
        }
    }
    if lost.is_empty() {
        return;
    }
    output::error(
        "these errors are the install resolve, not a broken gate: the code \
         compiles elsewhere in this run because a sibling workspace member \
         enables features the install packages lose when `cargo install` \
         resolves each package alone -",
    );
    for l in lost {
        output::error(&format!("  {l}"));
    }
    output::error(
        "the fix is usually declaring the feature in the install package's \
         own Cargo.toml.",
    );
}

/// Run the install-feature phase. `Ok(())` when inapplicable (no explicit
/// `[bin] install` list, or the mode rules this run out).
fn run_install_feature_phase(
    project_root: &Path,
    bin_cfg: Option<&crate::config::BinConfig>,
    certifies: Option<Certifies>,
    allow_flags: &[String],
    commands: bool,
) -> Result<(), DevError> {
    let Some(cfg) = bin_cfg.filter(|c| !c.install.is_empty()) else {
        return Ok(());
    };
    if !install_feature_applies(cfg.install_feature_check, certifies) {
        return Ok(());
    }

    // The same resolver `brokkr install` uses: package names against
    // discovered bin targets, unknown names refused in its words.
    let expected = crate::runnables::install_bin_targets(project_root, &cfg.install)?;

    // Lint allows reach this build the same way they reach the test phase -
    // env or `--config`, whichever layer is live (see `rustflags`).
    let (env_allows, allow_args) = rustflags::plumbing(project_root, false, allow_flags);
    let env_pairs = composed_rustflags_env(&[], env_allows);
    let env_refs: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mut args: Vec<String> = vec!["check".into()];
    args.extend(allow_args);
    args.extend(package_unification_args());
    // Every broken install package in one run, not just the first.
    args.push("--keep-going".into());
    // The profile install actually builds: release unless `[bin] debug`.
    if !cfg.debug {
        args.push("--release".into());
    }
    for (pkg, _) in &expected {
        args.push("-p".into());
        args.push(pkg.clone());
    }
    // `cargo install` installs every eligible bin in the package, so the
    // broad selector is right - the zero-target comparison below is what
    // keeps it honest about `required-features` skips.
    args.push("--bins".into());
    args.push("--message-format=json".into());

    if commands {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, &env_refs)?;
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr);

    if !captured.status.success() {
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        let events = cargo_json::parse_cargo_diagnostics(&stdout);
        let errors: Vec<_> = events.iter().filter(|d| d.level == "error").collect();
        if errors.is_empty() {
            // Nothing structured to show - a spawn-level or toolchain
            // failure. stderr carries cargo's own words.
            output::error(&stderr);
        } else {
            let mut msg = format!("install-feature: {}\n", output::count(errors.len(), "error"));
            for d in errors {
                msg.push_str("  ");
                msg.push_str(&event_to_clippy(d, false).format_one());
                msg.push('\n');
            }
            output::error(msg.trim_end());
        }
        if stderr.contains("feature-unification") || stderr.contains("nightly") {
            // No silent fallback: running un-pinned would recreate the
            // workspace-unified graph this phase exists to leave.
            output::error(
                "this cargo does not support -Zfeature-unification's \
                 \"package\" mode, which the install-feature phase needs to \
                 resolve each install package the way `cargo install` will. \
                 Update the nightly toolchain, or set \
                 `[bin] install_feature_check = \"off\"`.",
            );
        } else {
            let packages: Vec<String> = expected.iter().map(|(p, _)| p.clone()).collect();
            explain_lost_features(project_root, &packages, &env_refs);
        }
        return Err(DevError::Build("install-feature check failed".into()));
    }

    let missing = missing_bins(&expected, &checked_bins(&stdout));
    if !missing.is_empty() {
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        output::error(&format!(
            "install-feature: {} never compiled: {} - likely `required-features` \
             the package's own resolve does not satisfy, which `cargo install` \
             will hit the same way. A bin the phase cannot check cannot be \
             credited green.",
            output::count(missing.len(), "expected bin"),
            missing.join(", "),
        ));
        return Err(DevError::Build("install-feature check failed".into()));
    }

    let bins: usize = expected.iter().map(|(_, b)| b.len()).sum();
    output::run_msg(&format!(
        "install-feature: ok ({}, {}, resolved per package like `cargo install`; \
         shared lockfile, no codegen)",
        output::count(expected.len(), "package"),
        output::count(bins, "bin"),
    ));
    Ok(())
}

#[cfg(test)]
mod install_shape_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::config::InstallFeatureCheck as M;

    // Gate-only by default: package mode compiles duplicate dependency
    // variants, which belongs in the pre-landing run.
    #[test]
    fn the_default_mode_runs_only_under_a_complete_claim() {
        assert!(install_feature_applies(None, Some(Certifies::Complete)));
        assert!(!install_feature_applies(None, Some(Certifies::Partial)));
        assert!(!install_feature_applies(None, None));
    }

    #[test]
    fn explicit_modes_are_honored() {
        assert!(!install_feature_applies(Some(M::Off), Some(Certifies::Complete)));
        assert!(install_feature_applies(Some(M::Always), None));
        assert!(install_feature_applies(Some(M::Gate), Some(Certifies::Complete)));
        assert!(!install_feature_applies(Some(M::Gate), None));
    }

    // THE ZERO-TARGET REFUSAL. `--bins` silently skips a bin whose
    // `required-features` are unsatisfied, so "the invocation succeeded" is
    // not "the binaries were checked" - the artifact stream is the proof.
    #[test]
    fn a_bin_the_stream_never_produced_is_reported_missing() {
        let expected = vec![(
            "daemon-pkg".to_owned(),
            vec!["ba-daemon".to_owned(), "ba-mock-worker".to_owned()],
        )];
        let stream = concat!(
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/daemon#daemon-pkg@0.1.0","target":{"name":"ba-daemon","kind":["bin"]}}"#,
            "\n",
            // A lib artifact for the same package must not satisfy a bin.
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/daemon#daemon-pkg@0.1.0","target":{"name":"daemon","kind":["lib"]}}"#,
            "\n",
        );
        let missing = missing_bins(&expected, &checked_bins(stream));
        assert_eq!(missing, vec!["daemon-pkg/ba-mock-worker".to_owned()]);
    }

    #[test]
    fn a_fully_checked_package_reports_nothing_missing() {
        let expected = vec![("p".to_owned(), vec!["a".to_owned()])];
        let stream = concat!(
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/p#p@0.1.0","target":{"name":"a","kind":["bin"]}}"#,
            "\n",
        );
        assert!(missing_bins(&expected, &checked_bins(stream)).is_empty());
    }
}
