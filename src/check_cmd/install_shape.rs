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
// features reqwest contributed - and the reliance had its rationale written
// directly above the dependency line ("pulled through the network stack...
// the features are additive"), a comment load-bearing in the wrong
// direction. Prose that explains why a resolve is fine cannot be trusted in
// either direction; only the install-shaped resolve could falsify it, which
// is also why this header records MEASUREMENTS (below) rather than
// reasoning - so the next reader cannot argue their way back into the batch
// from first principles, the same move that comment made.
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
// the package's expected bin targets.
//
// A missing bin used to fail the phase outright, which was wrong in the
// other direction: `cargo install` installs the eligible bins and skips a
// feature-gated one without complaint, so a package with an optional admin
// binary alongside its ordinary one could never pass. The fix is NOT to
// decide eligibility here from `required-features` - see `probe_missing_bin`
// for why reproducing cargo's feature semantics would hide the very failure
// this refusal exists to catch - but to ask cargo about each missing bin by
// name and waive it only on cargo's own refusal. Waiving every bin in a
// package is still the no-op, and still fails.
//
// Gate-only by default (`[bin] install_feature_check`): package mode
// deliberately compiles duplicate variants of shared dependencies, which is
// priced for the pre-landing run, not the edit loop.

/// Cargo args pinning package-mode feature resolution - the resolution
/// boundary `cargo install` uses. CLI `--config` outranks a repo's committed
/// `.cargo/config.toml` pin (e.g. a workspace-mode pin adopted for the
/// parallel test lane), which is what lets both coexist.
/// One definition, shared with the `[[check]]` `feature_unification` key: this
/// phase and a package-mode test lane must mean the same thing by "the install
/// shape", or the lane's green would be evidence about a different build.
fn package_unification_args() -> [String; 3] {
    crate::config::CargoUnification::Package.cargo_args()
}

/// Did cargo reject the unification flags themselves, rather than fail to
/// compile something?
///
/// This one classification aborts the whole phase, so it has to be a
/// POSITIVE identification of cargo's own option rejection. The first
/// version tested `stderr.contains("nightly")`, which any rustc diagnostic
/// about an unstable API satisfies, as does `error: failed to select a
/// version for \`nightly-helper\`` - and the phase would then abort every
/// remaining install package with a false remedy. Absence of rustc
/// diagnostics is necessary (a real compile failure always has them) and
/// nowhere near sufficient: cargo fails before rustc for manifest,
/// resolution and config reasons that have nothing to do with these flags.
///
/// Wrong in the safe direction by construction: an unrecognised rejection
/// reads as an ordinary failure, which reports the package and continues.
fn is_toolchain_refusal(stderr: &str, has_rustc_errors: bool) -> bool {
    if has_rustc_errors {
        return false;
    }
    const REJECTIONS: [&str; 6] = [
        "flag is only accepted on the nightly channel",
        "unknown `-Z` flag",
        "unexpected argument '-Zfeature-unification'",
        "unexpected argument `-Zfeature-unification`",
        "feature-unification is unstable",
        "`feature-unification` is unstable",
    ];
    REJECTIONS.iter().any(|r| stderr.contains(r))
        || (stderr.contains("feature-unification") && stderr.contains("requires -Z"))
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

/// The invocation shape both the per-package check and the per-bin probe
/// use. Every resolution-affecting argument lives here so the probe cannot
/// answer a question about a different build than the one that asked it:
/// same unification config, same profile, same allows, same package.
fn install_check_args(
    package: &str,
    target_selector: &[String],
    debug: bool,
    allow_args: &[String],
) -> Vec<String> {
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
    args.extend(target_selector.iter().cloned());
    args.push("--message-format=json".into());
    args
}

/// What cargo says about one bin that the `--bins` stream never produced.
enum BinVerdict {
    /// Cargo built it when asked by name: eligible, and now checked.
    Checked,
    /// Cargo's own target-selection refusal: `required-features` this
    /// package's resolve does not satisfy, so `cargo install` would skip
    /// it too. The only verdict that waives a bin.
    Ineligible,
    /// Anything else - a compile error, or a failure nobody recognised.
    Failed,
}

/// Ask cargo whether a missing bin was ineligible or simply not built.
///
/// WHY AN ORACLE RATHER THAN A MODEL. The obvious fix for "the phase
/// demands a bin `cargo install` would skip" is to read `required-features`
/// from metadata and decide eligibility here. That reproduces cargo's
/// feature semantics inside brokkr - namespaced features, `dep:` suppressing
/// an implicit feature, weak dependency features, `dep/feat` in
/// `required-features` (which cargo does support), resolver differences -
/// and every one of those is a chance to decide a bin is ineligible when
/// cargo would have built it. That mistake is invisible: the bin leaves the
/// expected set, never compiles, and the phase reports green. It would move
/// cargo's silent skip - the exact defect the zero-target refusal exists to
/// catch - inside brokkr, where nothing is left to catch it.
///
/// So brokkr never decides. Broad selection (`--bins`) skips an ineligible
/// bin silently, but naming one (`--bin NAME`) makes cargo REFUSE it out
/// loud, and that refusal is the only thing that waives a bin here.
/// Everything else fails closed: a false red is visible and arguable, a
/// false green falsifies the phase's whole claim.
#[allow(clippy::too_many_arguments)]
fn probe_missing_bin(
    project_root: &Path,
    package: &str,
    bin: &str,
    debug: bool,
    allow_args: &[String],
    env_refs: &[(&str, &str)],
    raw: bool,
    commands: bool,
) -> Result<BinVerdict, DevError> {
    let selector = vec!["--bin".to_owned(), bin.to_owned()];
    let args = install_check_args(package, &selector, debug, allow_args);
    if commands {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, env_refs)?;
    if captured.status.success() {
        return Ok(BinVerdict::Checked);
    }

    let stdout = String::from_utf8_lossy(&captured.stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr);
    let events = cargo_json::parse_cargo_diagnostics(&stdout);
    let errors: Vec<_> = events.iter().filter(|d| d.level == "error").collect();

    // Cargo's target-selection refusal, and it must name THIS target -
    // a `requires the features` line about some other target is not an
    // answer to the question asked. If cargo ever rephrases this, the
    // match fails and the bin fails closed rather than being waived.
    if errors.is_empty()
        && stderr.contains("requires the features")
        && stderr.contains(&format!("`{bin}`"))
    {
        return Ok(BinVerdict::Ineligible);
    }

    output::error(&format!("failing command: cargo {}", args.join(" ")));
    if errors.is_empty() {
        output::error(&stderr);
    } else {
        report_errors(package, &errors, raw);
    }
    Ok(BinVerdict::Failed)
}

/// Render one invocation's rustc errors, whole under `--raw` and one line
/// each otherwise.
fn report_errors(package: &str, errors: &[&cargo_json::DiagnosticEvent], raw: bool) {
    if raw {
        // The full rustc diagnostics, not the one-line rendering. This
        // failure class is exactly where the dropped parts matter:
        // rustc's notes ("perhaps two different versions of crate X are
        // being used?"), spans, and help text name the mechanism the
        // one-liner cannot.
        output::error(&format!(
            "install-feature {package}: {}",
            output::count(errors.len(), "error")
        ));
        for d in errors {
            match &d.rendered {
                Some(r) => output::error(r.trim_end()),
                None => output::error(&event_to_clippy(d, false).format_one()),
            }
        }
        return;
    }
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
    raw: bool,
    commands: bool,
) -> Result<bool, DevError> {
    // `cargo install` installs every eligible bin in the package, so the
    // broad selector is right - the per-bin probe below is what keeps it
    // honest about `required-features` skips.
    let args = install_check_args(package, &["--bins".to_owned()], debug, allow_args);

    if commands {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, env_refs)?;
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr);

    if !captured.status.success() {
        let events = cargo_json::parse_cargo_diagnostics(&stdout);
        let errors: Vec<_> = events.iter().filter(|d| d.level == "error").collect();
        if is_toolchain_refusal(&stderr, !errors.is_empty()) {
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
        if errors.is_empty() {
            // Nothing structured to show - a spawn-level failure. stderr
            // carries cargo's own words.
            output::error(&stderr);
        } else {
            report_errors(package, &errors, raw);
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

    // A bin absent from the stream is not yet a failure and not yet a
    // waiver - cargo has not been asked about it. It is asked, one bin at
    // a time, and only its own refusal waives one.
    let missing = missing_bins(expected_bins, &scan_artifacts(&stdout, package).checked_bins);
    let mut ok = true;
    let mut waived: Vec<String> = Vec::new();
    for bin in &missing {
        match probe_missing_bin(
            project_root, package, bin, debug, allow_args, env_refs, raw, commands,
        )? {
            BinVerdict::Checked => {}
            BinVerdict::Ineligible => waived.push(bin.clone()),
            BinVerdict::Failed => ok = false,
        }
    }
    if !ok {
        return Ok(false);
    }
    if waived.len() == expected_bins.len() {
        // THE ZERO-TARGET REFUSAL, in its surviving form. Waiving bins one
        // by one on cargo's word is safe; waiving all of them leaves a
        // package that compiled no binary at all, which is the gate-shaped
        // no-op this refusal has always been about.
        output::error(&format!(
            "install-feature {package}: every declared bin is ineligible under \
             this package's own resolve ({}) - nothing was checked, so nothing \
             can be credited. `cargo install` would install no binary from this \
             package either.",
            waived.join(", "),
        ));
        return Ok(false);
    }
    if !waived.is_empty() {
        // Reported, not silent: a bin cargo declined to build is a bin this
        // phase did not check, and the next reader needs to know which
        // claim they are actually getting.
        output::run_msg(&format!(
            "install-feature {package}: {} skipped by cargo for unsatisfied \
             `required-features` ({}) - `cargo install` skips them the same way",
            output::count(waived.len(), "bin"),
            waived.join(", "),
        ));
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
    raw: bool,
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
            raw,
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

    // `--bins` silently skips a bin whose `required-features` are
    // unsatisfied, so "the invocation succeeded" is not "the binaries were
    // checked" - the artifact stream is the proof, and what it does not
    // account for is what gets put to cargo by name.
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

    // THE TOOLCHAIN REFUSAL ABORTS THE WHOLE PHASE, so it must be a
    // positive identification. The first version tested for the substring
    // "nightly", which ordinary compile output satisfies constantly.
    #[test]
    fn only_cargos_own_option_rejection_reads_as_an_unsupported_toolchain() {
        assert!(is_toolchain_refusal(
            "error: the `-Z` flag is only accepted on the nightly channel of Cargo",
            false
        ));
        assert!(is_toolchain_refusal("error: unknown `-Z` flag specified", false));

        // A dependency whose NAME contains the word, and no rustc errors:
        // the old classifier aborted every remaining package here.
        assert!(!is_toolchain_refusal(
            "error: failed to select a version for `nightly-helper`",
            false
        ));
        // An unstable-API diagnostic mentioning nightly, with rustc errors
        // present - an ordinary compile failure, reported per package.
        assert!(!is_toolchain_refusal(
            "error[E0658]: use of unstable library feature; add `#![feature(x)]` \
             to the crate attributes to enable (nightly only)",
            true
        ));
        // Even the right words do not qualify once rustc has spoken.
        assert!(!is_toolchain_refusal(
            "error: unknown `-Z` flag specified",
            true
        ));
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
