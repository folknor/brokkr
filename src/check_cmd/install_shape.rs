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
// path, which is the worst place to learn about it. First production run of
// this phase caught exactly that: a web bridge relying on hyper-util
// features reqwest contributed, with the reliance written down in a manifest
// comment as a saving.
//
// ONE INVOCATION PER PACKAGE, deliberately. RFC 3692 describes multi-`-p`
// package mode as equivalent to N separate builds, and measured against a
// real workspace that is false: cargo still DEDUPES UNITS across the batched
// graph. With two variants of a shared dep in the batch (one member's
// resolve wants `arrow`, another's does not), a path dependency whose own
// features do not vary dedupes onto one variant while a crate between them
// compiles against both - and types with exactly one definition become
// "expected `AccountId`, found `AccountId`" errors in code no install
// resolve would ever produce. Five such phantoms shipped in this phase's
// first report. `cargo install` resolves one package at a time, so one
// invocation per package IS the advertised boundary - and it attributes
// every error to the package whose resolve produced it for free, where the
// batch pointed at an innocent intermediate crate.
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

/// The bin targets the artifact stream must contain for a package to be
/// credited.
fn missing_bins(
    expected: &[String],
    checked: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    expected
        .iter()
        .filter(|b| !checked.contains(*b))
        .cloned()
        .collect()
}

/// One invocation's artifact stream, reduced to what the refusals need:
/// which bin targets were actually checked, and whether any dependency
/// compiled as two feature variants inside this single resolve.
struct ArtifactScan {
    checked_bins: std::collections::BTreeSet<String>,
    /// `name@version` of dependencies whose lib target appeared with more
    /// than one feature set in one invocation. Within a single-package
    /// resolve this should not happen; when it does, rustc's resulting
    /// "expected `X`, found `X`" errors are illegible without this name.
    duplicate_units: Vec<String>,
}

/// Parse cargo's artifact stream for one package's invocation.
fn scan_artifacts(stdout: &str, package: &str) -> ArtifactScan {
    #[derive(serde::Deserialize)]
    struct Artifact {
        reason: String,
        package_id: String,
        target: ArtifactTarget,
        #[serde(default)]
        features: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct ArtifactTarget {
        name: String,
        kind: Vec<String>,
    }

    let mut checked_bins = std::collections::BTreeSet::new();
    let mut lib_features: std::collections::BTreeMap<String, std::collections::BTreeSet<Vec<String>>> =
        std::collections::BTreeMap::new();
    for line in stdout.lines() {
        let Ok(a) = serde_json::from_str::<Artifact>(line) else {
            continue;
        };
        if a.reason != "compiler-artifact" {
            continue;
        }
        let owner = package_name_from_id(&a.package_id);
        if a.target.kind.iter().any(|k| k == "bin") {
            if owner == package {
                checked_bins.insert(a.target.name);
            }
        } else if a.target.kind.iter().any(|k| k == "lib") {
            // Lib targets only: build scripts and proc-macros legitimately
            // resolve their own feature domain and are not the confusion
            // this note exists for.
            let mut features = a.features;
            features.sort();
            lib_features.entry(a.package_id).or_default().insert(features);
        }
    }
    let duplicate_units = lib_features
        .into_iter()
        .filter(|(_, sets)| sets.len() > 1)
        .map(|(id, _)| package_name_from_id(&id))
        .collect();
    ArtifactScan {
        checked_bins,
        duplicate_units,
    }
}

/// Best-effort attribution for a package-mode failure: which features does
/// the workspace resolve enable that this package's solo resolve does not?
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
fn explain_lost_features(project_root: &Path, package: &str, env: &[(&str, &str)]) {
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
    scoped.push("-p");
    scoped.push(package);
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
    output::error(&format!(
        "these errors are {package}'s install resolve, not a broken gate: the \
         code compiles elsewhere in this run because a sibling workspace \
         member enables features {package} loses when `cargo install` \
         resolves it alone -",
    ));
    for l in lost {
        output::error(&format!("  {l}"));
    }
    output::error(
        "the fix is usually declaring the feature in the install package's \
         own Cargo.toml.",
    );
}

/// Check one install package under its own package-mode resolve. Returns
/// whether it passed; failures are already reported, attributed to this
/// package. `Err` is reserved for the unsupported-toolchain refusal, which
/// would repeat identically for every remaining package.
#[allow(clippy::too_many_arguments)]
fn check_one_install_package(
    project_root: &Path,
    package: &str,
    expected_bins: &[String],
    debug: bool,
    allow_args: &[String],
    env_refs: &[(&str, &str)],
    commands: bool,
) -> Result<bool, DevError> {
    let mut args: Vec<String> = vec!["check".into()];
    args.extend(allow_args.iter().cloned());
    args.extend(package_unification_args());
    args.push("--keep-going".into());
    // The profile install actually builds: release unless `[bin] debug`.
    if !debug {
        args.push("--release".into());
    }
    args.push("-p".into());
    args.push(package.to_owned());
    // `cargo install` installs every eligible bin in the package, so the
    // broad selector is right - the zero-target comparison below is what
    // keeps it honest about `required-features` skips.
    args.push("--bins".into());
    args.push("--message-format=json".into());

    if commands {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, env_refs)?;
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr);

    if !captured.status.success() {
        if stderr.contains("feature-unification") || stderr.contains("nightly") {
            // No silent fallback: running un-pinned would recreate the
            // workspace-unified graph this phase exists to leave. A hard
            // stop rather than a per-package failure - it would repeat
            // verbatim for every remaining package.
            output::error(&format!("failing command: cargo {}", args.join(" ")));
            output::error(&stderr);
            return Err(DevError::Build(
                "this cargo does not support -Zfeature-unification's \
                 \"package\" mode, which the install-feature phase needs to \
                 resolve each install package the way `cargo install` will. \
                 Update the nightly toolchain, or set \
                 `[bin] install_feature_check = \"off\"`."
                    .into(),
            ));
        }
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        let events = cargo_json::parse_cargo_diagnostics(&stdout);
        let errors: Vec<_> = events.iter().filter(|d| d.level == "error").collect();
        if errors.is_empty() {
            // Nothing structured to show - a spawn-level failure. stderr
            // carries cargo's own words.
            output::error(&stderr);
        } else {
            let mut msg = format!(
                "install-feature {package}: {}\n",
                output::count(errors.len(), "error")
            );
            for d in errors {
                msg.push_str("  ");
                msg.push_str(&event_to_clippy(d, false).format_one());
                msg.push('\n');
            }
            output::error(msg.trim_end());
        }
        let scan = scan_artifacts(&stdout, package);
        if !scan.duplicate_units.is_empty() {
            // Should be impossible in a single-package resolve; when it
            // happens anyway, rustc's "expected `X`, found `X`" output sends
            // readers hunting version skew that does not exist.
            output::error(&format!(
                "note: this resolve built two feature variants of: {} - \
                 \"mismatched types\" between identical type names is that, \
                 not version skew",
                scan.duplicate_units.join(", "),
            ));
        }
        explain_lost_features(project_root, package, env_refs);
        return Ok(false);
    }

    let missing = missing_bins(expected_bins, &scan_artifacts(&stdout, package).checked_bins);
    if !missing.is_empty() {
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        output::error(&format!(
            "install-feature {package}: {} never compiled: {} - likely \
             `required-features` the package's own resolve does not satisfy, \
             which `cargo install` will hit the same way. A bin the phase \
             cannot check cannot be credited green.",
            output::count(missing.len(), "expected bin"),
            missing.join(", "),
        ));
        return Ok(false);
    }
    Ok(true)
}

/// Run the install-feature phase. `Ok(())` when inapplicable (no explicit
/// `[bin] install` list, or the mode rules this run out).
///
/// One cargo invocation per install package - the batched multi-`-p` form
/// is NOT equivalent (see the module header) - so every package is checked
/// and reported even when an earlier one fails.
fn run_install_feature_phase(
    project_root: &Path,
    bin_cfg: Option<&crate::config::BinConfig>,
    cli_packages: &[String],
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

    // CLI `-p` intersects, matching every other phase: named install
    // packages are checked, the rest dropped with a note, and an
    // intersection that empties skips the phase visibly. (Under the gate
    // this is unreachable - a complete profile refuses `-p` outright.)
    let selected: Vec<String> = if cli_packages.is_empty() {
        cfg.install.clone()
    } else {
        cfg.install
            .iter()
            .filter(|p| cli_packages.contains(p))
            .cloned()
            .collect()
    };
    if selected.is_empty() {
        output::run_msg("install-feature: skipped (-p rules the install set out)");
        return Ok(());
    }
    if selected.len() < cfg.install.len() {
        output::run_msg(&format!(
            "install-feature: -p narrows the install set to {}",
            selected.join(", ")
        ));
    }

    // The same resolver `brokkr install` uses: package names against
    // discovered bin targets, unknown names refused in its words.
    let expected = crate::runnables::install_bin_targets(project_root, &selected)?;

    // Lint allows reach this build the same way they reach the test phase -
    // env or `--config`, whichever layer is live (see `rustflags`).
    let (env_allows, allow_args) = rustflags::plumbing(project_root, false, allow_flags);
    let env_pairs = composed_rustflags_env(&[], env_allows);
    let env_refs: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mut ok = true;
    for (pkg, bins) in &expected {
        ok &= check_one_install_package(
            project_root,
            pkg,
            bins,
            cfg.debug,
            &allow_args,
            &env_refs,
            commands,
        )?;
    }
    if !ok {
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
        let expected = vec!["ba-daemon".to_owned(), "ba-mock-worker".to_owned()];
        let stream = concat!(
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/daemon#daemon-pkg@0.1.0","target":{"name":"ba-daemon","kind":["bin"]}}"#,
            "\n",
            // A lib artifact for the same package must not satisfy a bin.
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/daemon#daemon-pkg@0.1.0","target":{"name":"daemon","kind":["lib"]}}"#,
            "\n",
        );
        let scan = scan_artifacts(stream, "daemon-pkg");
        assert_eq!(
            missing_bins(&expected, &scan.checked_bins),
            vec!["ba-mock-worker".to_owned()]
        );
    }

    // A sibling package's bin in the stream must not satisfy this package's
    // expectation - attribution is per package now, and the scan follows.
    #[test]
    fn another_packages_bin_does_not_satisfy_this_package() {
        let stream = concat!(
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/other#other@0.1.0","target":{"name":"ba-daemon","kind":["bin"]}}"#,
            "\n",
        );
        let scan = scan_artifacts(stream, "daemon-pkg");
        assert_eq!(
            missing_bins(&["ba-daemon".to_owned()], &scan.checked_bins),
            vec!["ba-daemon".to_owned()]
        );
    }

    // THE DUPLICATE-UNIT NOTE. One crate version compiled under two feature
    // sets in a single resolve produces rustc's least legible error shape
    // ("expected `OmsType`, found a different `OmsType`"), so the scan names
    // it. Lib targets only: a build script resolving its own feature domain
    // is normal, not this.
    #[test]
    fn two_feature_variants_of_one_lib_are_named() {
        let stream = concat!(
            r#"{"reason":"compiler-artifact","package_id":"registry+https://x#nautilus-model@0.62.0","target":{"name":"nautilus-model","kind":["lib"]},"features":["arrow"]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"registry+https://x#nautilus-model@0.62.0","target":{"name":"nautilus-model","kind":["lib"]},"features":[]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"registry+https://x#serde@1.0.0","target":{"name":"serde","kind":["lib"]},"features":["std"]}"#,
            "\n",
            // Same features twice (e.g. cargo re-reporting a fresh unit):
            // not a duplicate variant.
            r#"{"reason":"compiler-artifact","package_id":"registry+https://x#serde@1.0.0","target":{"name":"serde","kind":["lib"]},"features":["std"]}"#,
            "\n",
        );
        let scan = scan_artifacts(stream, "whatever");
        assert_eq!(scan.duplicate_units, vec!["nautilus-model".to_owned()]);
    }
}
