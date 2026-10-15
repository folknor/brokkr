// The member-check phase: compile every workspace member's production
// targets the way `cargo install -p <member>` will resolve them.
//
// WHY THE WORKSPACE'S GREEN DOES NOT COVER THIS. Every other compiling phase
// resolves features over a selection that includes sibling workspace members,
// so the feature set of a shared dependency is the UNION of what all members
// ask for. A member resolved alone loses what its siblings donated. A call
// site that only compiles because a sibling enables a third-party feature
// (tokio's, hyper's, a nautilus crate's) is green under every
// workspace-selected sweep and fails at install time - on the deploy path,
// which is the worst place to learn about it. The first production run of
// this phase (then install-only) caught exactly that: a web bridge relying
// on hyper-util features reqwest contributed - and the reliance had its
// rationale written directly above the dependency line ("pulled through the
// network stack... the features are additive"), a comment load-bearing in
// the wrong direction. Prose that explains why a resolve is fine cannot be
// trusted in either direction; only the isolated resolve could falsify it,
// which is also why this header records MEASUREMENTS (below) rather than
// reasoning - so the next reader cannot argue their way back into the batch
// from first principles, the same move that comment made.
//
// PRODUCTION TARGETS ONLY. `targets` spells lib and bins, nothing else.
// Selecting tests, benches or examples pulls the member's dev-dependencies
// into the same resolve, and a dev-dependency can donate a feature to a
// dependency the production targets also use - package unification isolates
// members from each other, not a member's targets from each other. A dev
// target in this phase would reintroduce, inside one member, the masking the
// phase exists to remove across members.
//
// ONE INVOCATION PER MEMBER, deliberately. RFC 3692 describes multi-`-p`
// package mode as equivalent to N separate builds, and measured against a
// real workspace that is false: cargo still DEDUPES UNITS across the batched
// graph. With two variants of a shared dep in the batch (one member's
// resolve wants `arrow`, another's does not), a path dependency whose own
// features do not vary dedupes onto one variant while a crate between them
// compiles against both - and types with exactly one definition become
// "expected `AccountId`, found `AccountId`" errors in code no isolated
// resolve would ever produce. Five such phantoms shipped in the first
// report. `cargo install` resolves one package at a time, so one invocation
// per member IS the advertised boundary - and it attributes every error to
// the member whose resolve produced it for free.
//
// WHAT A GREEN RUN CLAIMS, PRECISELY: each checked member's selected
// production targets compile under that member's own package-mode resolve,
// at the named profile, against the workspace lockfile (`--locked`). It does
// NOT claim the members will `cargo install`: `cargo check` performs no
// codegen or linking, and the profile checked is the one named, not
// necessarily install's.
//
// THE ZERO-TARGET REFUSAL is not optional. `--bins` silently skips a bin
// whose `required-features` are unsatisfied, so a member could be "checked"
// with none of its targets compiled - a gate-shaped no-op. The artifact
// stream is compared against the member's expected targets, keyed by
// (package id, class, name) and crediting only non-test compiles. A missing
// bin is put to cargo by name and waived only on cargo's own refusal - see
// `probe_missing_bin` for why brokkr never decides eligibility itself.
// Waiving every target of a member is still the no-op, and still fails.

use crate::config::{MemberCheckConfig, MemberProfile, MemberSkip, MemberTarget};

/// Cargo args pinning package-mode feature resolution - the resolution
/// boundary `cargo install` uses. CLI `--config` outranks a repo's committed
/// `.cargo/config.toml` pin (e.g. a workspace-mode pin adopted for the
/// parallel test lane), which is what lets both coexist.
/// One definition, shared with the `[[check]]` `feature_unification` key: this
/// phase and a package-mode test lane must mean the same thing by "the install
/// shape", or the lane's green would be evidence about a different build.
fn package_unification_args() -> [String; 3] {
    crate::config::check::CargoUnification::Package.cargo_args()
}

/// The isolated resolve has the same cache boundary as a package-unification
/// sweep without per-sweep rustflags. Keep it in the profile's canonical
/// target-location model so the directory name cannot drift from
/// `PlannedSweep::target_location`.
fn member_target_dir(meta_target_dir: &std::path::Path) -> std::path::PathBuf {
    crate::profile::TargetLocation::Isolated(crate::profile::IsolatedTargetKey::Unify {
        mode: crate::config::check::CargoUnification::Package,
        rustflags_hash: None,
    })
    .target_dir(meta_target_dir)
}

/// The members the phase checks, and a note per `-p` name it did not admit.
///
/// Skip names are validated against the WHOLE member set before any `-p`
/// intersection: a narrowed invocation must not hide a stale entry.
fn admit_members(
    members: &[String],
    skip: &[MemberSkip],
    cli: &[String],
) -> Result<(Vec<String>, Vec<String>), DevError> {
    let stale: Vec<&str> = skip
        .iter()
        .filter(|s| !members.contains(&s.package))
        .map(|s| s.package.as_str())
        .collect();
    if !stale.is_empty() {
        return Err(DevError::Config(format!(
            "[member_check] skip names {} - not a workspace member. Remove the entry.",
            stale.join(", ")
        )));
    }
    let skipped = |p: &String| skip.iter().find(|s| &s.package == p);
    if cli.is_empty() {
        return Ok((members.iter().filter(|m| skipped(m).is_none()).cloned().collect(), Vec::new()));
    }
    let mut kept = Vec::new();
    let mut notes = Vec::new();
    for p in cli {
        if kept.contains(p) {
            continue;
        }
        if let Some(s) = skipped(p) {
            notes.push(format!("-p {p} is skipped by [member_check] ({})", s.reason));
        } else if !members.contains(p) {
            notes.push(format!("-p {p} is not a workspace member"));
        } else {
            kept.push(p.clone());
        }
    }
    Ok((kept, notes))
}

/// A production target class. `Lib` is every metadata kind of the library
/// family (`lib`, `rlib`, `dylib`, `cdylib`, `staticlib`, `proc-macro`) -
/// cargo spells the one library target by its crate types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TargetClass {
    Lib,
    Bin,
}

impl TargetClass {
    fn of(kinds: &[String]) -> Option<Self> {
        const LIB_KINDS: [&str; 6] = ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];
        if kinds.iter().any(|k| k == "bin") {
            Some(Self::Bin)
        } else if kinds.iter().any(|k| LIB_KINDS.contains(&k.as_str())) {
            Some(Self::Lib)
        } else {
            None
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Lib => "lib",
            Self::Bin => "bin",
        }
    }
}

/// One member as the phase sees it: its exact package id (artifact
/// attribution keys on it, never the name) and the production targets the
/// selection expects.
#[derive(Debug, Clone)]
struct MemberPlan {
    name: String,
    id: String,
    expected: Vec<(TargetClass, String)>,
}

impl MemberPlan {
    /// The broad selectors for this member: `--lib` and/or `--bins`, only
    /// for the classes it actually has.
    fn selectors(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.expected.iter().any(|(c, _)| *c == TargetClass::Lib) {
            out.push("--lib".into());
        }
        if self.expected.iter().any(|(c, _)| *c == TargetClass::Bin) {
            out.push("--bins".into());
        }
        out
    }
}

/// The plan for each admitted member, from one workspace metadata snapshot.
fn plan_members(
    metadata: &crate::cargo::metadata::CargoMetadata,
    admitted: &[String],
    targets: &[MemberTarget],
) -> Vec<MemberPlan> {
    let want_lib = targets.contains(&MemberTarget::Lib);
    let want_bins = targets.contains(&MemberTarget::Bins);
    admitted
        .iter()
        .filter_map(|name| {
            let pkg = metadata
                .packages
                .iter()
                .find(|p| &p.name == name && metadata.is_member(&p.id))?;
            let expected = pkg
                .targets
                .iter()
                .filter_map(|t| {
                    let class = TargetClass::of(&t.kind)?;
                    let wanted = match class {
                        TargetClass::Lib => want_lib,
                        TargetClass::Bin => want_bins,
                    };
                    wanted.then(|| (class, t.name.clone()))
                })
                .collect();
            Some(MemberPlan { name: name.clone(), id: pkg.id.to_string(), expected })
        })
        .collect()
}

/// One invocation's artifact stream, reduced to what the refusals need:
/// which of the member's production targets were actually checked, and
/// which dependencies compiled as more than one feature set.
struct ArtifactScan {
    checked: std::collections::BTreeSet<(TargetClass, String)>,
    /// `name@version` of dependencies whose lib target appeared with more
    /// than one feature set in one invocation. Not an error in itself -
    /// host and target dependency domains may legitimately differ - but the
    /// name is what makes rustc's "expected `X`, found `X`" legible when it
    /// is the cause.
    multi_variant: Vec<String>,
}

/// Parse cargo's artifact stream for one member's invocation. Only a
/// non-test compile (`profile.test == false`) of the member's own package
/// id credits a target.
fn scan_artifacts(stdout: &str, package_id: &str) -> ArtifactScan {
    #[derive(serde::Deserialize)]
    struct Artifact {
        reason: String,
        package_id: String,
        target: ArtifactTarget,
        #[serde(default)]
        features: Vec<String>,
        #[serde(default)]
        profile: Option<ArtifactProfile>,
    }
    #[derive(serde::Deserialize)]
    struct ArtifactTarget {
        name: String,
        kind: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct ArtifactProfile {
        #[serde(default)]
        test: bool,
    }

    let mut checked = std::collections::BTreeSet::new();
    let mut lib_features: std::collections::BTreeMap<String, std::collections::BTreeSet<Vec<String>>> =
        std::collections::BTreeMap::new();
    for line in stdout.lines() {
        let Ok(a) = serde_json::from_str::<Artifact>(line) else {
            continue;
        };
        if a.reason != "compiler-artifact" {
            continue;
        }
        let class = TargetClass::of(&a.target.kind);
        if a.package_id == package_id {
            if let Some(class) = class
                && !a.profile.is_some_and(|p| p.test)
            {
                checked.insert((class, a.target.name));
            }
        } else if class == Some(TargetClass::Lib) {
            let mut features = a.features;
            features.sort();
            lib_features.entry(a.package_id).or_default().insert(features);
        }
    }
    let multi_variant = lib_features
        .into_iter()
        .filter(|(_, sets)| sets.len() > 1)
        .map(|(id, _)| package_name_from_id(&id))
        .collect();
    ArtifactScan { checked, multi_variant }
}

/// A candidate explanation for a member-check failure: which features does
/// the workspace resolve enable that this member's solo resolve does not?
///
/// A member-check error is an ordinary rustc error against a call site that
/// compiles fine in the rest of the run, so the first person to hit one will
/// reasonably assume the gate is broken. The lost features are usually why -
/// but not provably: the error could be unconditional, and the comparison is
/// over cargo tree's normal+build graph with default features, not the exact
/// target selection that failed. So it is offered as a candidate, never as a
/// diagnosis.
///
/// Best-effort in every direction: a `cargo tree` failure degrades to
/// nothing rather than masking the compile error it was meant to explain.
fn explain_lost_features(project_root: &Path, package: &str, env: &[(&str, &str)]) {
    let base = [
        "tree",
        "--locked",
        "-e",
        "features,normal,build",
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
        let captured = run_check_cargo(args, project_root, env).ok()?;
        if !captured.success() {
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
        "candidate explanation: {package} resolved alone loses features a sibling \
         workspace member enables in the workspace resolve (normal+build graph, \
         default features - approximate) -",
    ));
    for l in lost {
        output::error(&format!("  {l}"));
    }
    output::error(
        "if one of these is what the failing code needs, declare it in the \
         member's own Cargo.toml.",
    );
}

/// The invocation shape both the per-member check and the per-bin probe
/// use. Every resolution-affecting argument lives here so the probe cannot
/// answer a question about a different build than the one that asked it:
/// same unification config, same profile, same allows, same package.
fn member_check_args(
    package: &str,
    target_selector: &[String],
    profile: MemberProfile,
    allow_args: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = vec!["check".into()];
    args.extend(allow_args.iter().cloned());
    args.extend(package_unification_args());
    args.push("--keep-going".into());
    args.push("--locked".into());
    let shape = crate::build_purpose::resolve(crate::build_purpose::Request {
        purpose: crate::build_purpose::Purpose::MemberCheck,
        profile: if profile == MemberProfile::Dev {
            crate::build_purpose::Profile::Dev
        } else {
            crate::build_purpose::Profile::Release
        },
        packages: vec!["-p".into(), package.to_owned()],
        features: crate::build_purpose::Features::default(),
        unification: Vec::new(),
        targets: target_selector.to_vec(),
        env: Vec::new(),
        build_root: None,
    });
    args.extend(shape.args);
    args.push("--message-format=json".into());
    args
}

/// What cargo says about one bin that the `--bins` stream never produced.
enum BinVerdict {
    /// Cargo built it when asked by name, and its artifact is in the stream.
    Checked,
    /// Cargo's own target-selection refusal: `required-features` this
    /// member's resolve does not satisfy, so `cargo install` would skip it
    /// too. The only verdict that waives a bin.
    Ineligible,
    /// Anything else - a compile error, a success with no artifact, or a
    /// failure nobody recognised.
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
/// loud, and that refusal is the only thing that waives a bin here. A
/// success must also carry the bin's artifact - the broad invocation is held
/// to that evidence, so the probe is too. Everything else fails closed: a
/// false red is visible and arguable, a false green falsifies the phase's
/// whole claim.
fn probe_missing_bin(
    project_root: &Path,
    member: &MemberPlan,
    bin: &str,
    profile: MemberProfile,
    allow_args: &[String],
    env_refs: &[(&str, &str)],
    commands: bool,
) -> Result<BinVerdict, DevError> {
    let selector = vec!["--bin".to_owned(), bin.to_owned()];
    let args = member_check_args(&member.name, &selector, profile, allow_args);
    cargo_line(commands, &format!("cargo {}", args.join(" ")));
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = run_check_cargo(&arg_refs, project_root, env_refs)?;
    let stdout = String::from_utf8_lossy(&captured.stdout);
    if captured.success() {
        if scan_artifacts(&stdout, &member.id).checked.contains(&(TargetClass::Bin, bin.to_owned())) {
            return Ok(BinVerdict::Checked);
        }
        output::error(&format!(
            "member-check {}: cargo accepted --bin {bin} but no artifact for it reached the stream",
            member.name
        ));
        return Ok(BinVerdict::Failed);
    }

    let stderr = String::from_utf8_lossy(&captured.stderr);
    let events = cargo_json::parse_cargo_diagnostics(&stdout);
    let errors: Vec<_> = events.iter().filter(|d| d.is_error()).collect();

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
        report_errors(&member.name, &errors);
    }
    Ok(BinVerdict::Failed)
}

/// Render one invocation's rustc errors, one line each.
fn report_errors(package: &str, errors: &[&Diagnostic]) {
    let mut msg = format!(
        "member-check {package}: {}\n",
        output::count(errors.len(), "error")
    );
    for d in errors {
        msg.push_str("  ");
        msg.push_str(&d.format_one());
        msg.push('\n');
    }
    output::error(msg.trim_end());
}

/// Check one member under its own package-mode resolve. Returns whether it
/// passed; failures are already reported, attributed to this member. `Err`
/// is reserved for the unsupported-toolchain refusal, which would repeat
/// identically for every remaining member.
fn check_one_member(
    project_root: &Path,
    member: &MemberPlan,
    profile: MemberProfile,
    allow_args: &[String],
    env_refs: &[(&str, &str)],
    commands: bool,
) -> Result<bool, DevError> {
    // The running status names the member in flight, so a watchdog report
    // attributes a timeout to it.
    output::status(&format!("member-check {}", member.name));
    if member.expected.is_empty() {
        // A member with no target of any selected class compiles nothing -
        // the gate-shaped no-op. Skipping it is a decision someone writes down.
        output::error(&format!(
            "member-check {}: no target of the selected classes - nothing would be \
             checked. Add it to [member_check] skip with a reason.",
            member.name
        ));
        return Ok(false);
    }
    let args = member_check_args(&member.name, &member.selectors(), profile, allow_args);
    cargo_line(commands, &format!("cargo {}", args.join(" ")));
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = run_check_cargo(&arg_refs, project_root, env_refs)?;
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr);

    if !captured.success() {
        let events = cargo_json::parse_cargo_diagnostics(&stdout);
        let errors: Vec<_> = events.iter().filter(|d| d.is_error()).collect();
        if is_feature_unification_refusal(&stderr, !errors.is_empty()) {
            // No silent fallback: running un-pinned would recreate the
            // workspace-unified graph this phase exists to leave. A hard
            // stop rather than a per-member failure - it would repeat
            // verbatim for every remaining member.
            output::error(&format!("failing command: cargo {}", args.join(" ")));
            output::error(&stderr);
            return Err(DevError::Build(
                "this cargo does not support -Zfeature-unification's \
                 \"package\" mode, which the member-check phase needs to \
                 resolve each member the way `cargo install` will. \
                 Update the nightly toolchain, or set \
                 `[member_check] when = \"off\"` (`[bin] install_feature_check \
                 = \"off\"` for the install gate)."
                    .into(),
            ));
        }
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        if errors.is_empty() {
            // Nothing structured to show - a spawn-level or resolve failure.
            // stderr carries cargo's own words.
            output::error(&stderr);
        } else {
            report_errors(&member.name, &errors);
        }
        let scan = scan_artifacts(&stdout, &member.id);
        if !scan.multi_variant.is_empty() {
            output::error(&format!(
                "note: this resolve built more than one feature set of: {} - if rustc \
                 reports mismatched types between identical type names, this is a \
                 candidate cause",
                scan.multi_variant.join(", "),
            ));
        }
        explain_lost_features(project_root, &member.name, env_refs);
        return Ok(false);
    }

    // A target absent from the stream is not yet a failure and not yet a
    // waiver. A missing lib has no `required-features` to excuse it, so it
    // fails; a missing bin is put to cargo by name, and only cargo's own
    // refusal waives it.
    let checked = scan_artifacts(&stdout, &member.id).checked;
    let mut ok = true;
    let mut waived: Vec<String> = Vec::new();
    for (class, name) in member.expected.iter().filter(|t| !checked.contains(*t)) {
        match class {
            TargetClass::Lib => {
                output::error(&format!(
                    "member-check {}: cargo succeeded but the lib target {name} never \
                     reached the artifact stream",
                    member.name
                ));
                ok = false;
            }
            TargetClass::Bin => match probe_missing_bin(
                project_root, member, name, profile, allow_args, env_refs, commands,
            )? {
                BinVerdict::Checked => {}
                BinVerdict::Ineligible => waived.push(name.clone()),
                BinVerdict::Failed => ok = false,
            },
        }
    }
    if !ok {
        return Ok(false);
    }
    if waived.len() == member.expected.len() {
        // THE ZERO-TARGET REFUSAL, in its surviving form. Waiving bins one
        // by one on cargo's word is safe; waiving all of them leaves a
        // member that compiled nothing at all.
        output::error(&format!(
            "member-check {}: every selected target is ineligible under this member's \
             own resolve ({}) - nothing was checked, so nothing can be credited.",
            member.name,
            waived.join(", "),
        ));
        return Ok(false);
    }
    if !waived.is_empty() {
        // Reported, not silent: a bin cargo declined to build is a bin this
        // phase did not check.
        output::run_msg(&format!(
            "member-check {}: {} skipped by cargo for unsatisfied \
             `required-features` ({}) - `cargo install` skips them the same way",
            member.name,
            output::count(waived.len(), "bin"),
            waived.join(", "),
        ));
    }
    Ok(true)
}

/// Run the member-check phase. `Ok(())` when inapplicable; a visible skip
/// when `-p` rules every member out.
///
/// One cargo invocation per member - the batched multi-`-p` form is NOT
/// equivalent (see the module header) - so every member is checked and
/// reported even when an earlier one fails.
fn run_member_check_phase(
    project_root: &Path,
    selection: &MemberSelection,
    allow_flags: &[String],
    commands: bool,
) -> Result<(), DevError> {
    let (cfg, cli): (&MemberCheckConfig, &[String]) = match selection {
        MemberSelection::NotApplicable(reason) => {
            output::detail(&format!("member-check: not applicable ({reason})"));
            return Ok(());
        }
        MemberSelection::Eligible { cfg, cli } => (cfg, cli),
    };

    let metadata = crate::cargo::metadata::load(
        project_root,
        crate::cargo::metadata::Query::workspace(),
        &[],
        Some(Phase::MemberCheck),
    )?;
    let workspace: Vec<String> = metadata
        .packages
        .iter()
        .filter(|p| metadata.is_member(&p.id))
        .map(|p| p.name.clone())
        .collect();
    // Implied by `[bin] install`: the install packages are the member set.
    let members = match &cfg.only {
        Some(only) => {
            if let Some(unknown) = only.iter().find(|p| !workspace.contains(p)) {
                return Err(DevError::Config(format!(
                    "[bin] install names '{unknown}', which is not a workspace member"
                )));
            }
            only.clone()
        }
        None => workspace,
    };
    let configured = members.len() - cfg.skip.len().min(members.len());
    let (admitted, notes) = admit_members(&members, &cfg.skip, cli)?;
    for note in &notes {
        output::detail(&format!("member-check: {note}"));
    }
    if admitted.is_empty() {
        output::run_msg("member-check: skipped (-p rules every checked member out)");
        return Ok(());
    }
    let plans = plan_members(&metadata, &admitted, &cfg.targets);

    // Lint allows reach this build the same way they reach the test phase -
    // env or `--config`, whichever layer is live (see `rustflags`).
    let (env_allows, allow_args) = rustflags::plumbing(project_root, false, allow_flags);
    let target_dir = member_target_dir(&build::project_info(Some(project_root))?.target_dir);
    let mut env_pairs = rustflags::RustflagsEnv::process()
        .compose(env_allows)?
        .into_iter()
        .collect::<Vec<_>>();
    // Both the broad build and every missing-bin probe share this env, so
    // they ask Cargo about the same package-mode artifact cache.
    env_pairs.push((
        "CARGO_TARGET_DIR".into(),
        target_dir.to_string_lossy().into_owned(),
    ));
    let env_refs: Vec<(&str, &str)> = env_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mut ok = true;
    for plan in &plans {
        ok &= check_one_member(project_root, plan, cfg.profile, &allow_args, &env_refs, commands)?;
    }
    if !ok {
        // Every failing member already reported itself above.
        return Err(DevError::Reported("member-check failed".into()));
    }

    // The explanation never changes, so it is log-only; the counts are the
    // run's own facts.
    output::detail(
        "member-check: each member's production targets resolved on their own like \
         `cargo install`; shared lockfile, no codegen",
    );
    output::run_msg(&format!(
        "member-check: ok ({}) in {}",
        member_counts(&plans, configured, cfg.profile),
        fmt_wall(phase_elapsed())
    ));
    Ok(())
}

/// `3 members, 2 libs, 4 bins, dev`, or `1 of 3 members, ...` when `-p`
/// narrowed the set.
fn member_counts(plans: &[MemberPlan], configured: usize, profile: MemberProfile) -> String {
    let members = if plans.len() < configured {
        format!("{} of {}", plans.len(), output::count(configured, "member"))
    } else {
        output::count(plans.len(), "member")
    };
    let count = |class| plans.iter().flat_map(|p| &p.expected).filter(|(c, _)| *c == class).count();
    let profile = match profile {
        MemberProfile::Dev => "dev",
        MemberProfile::Release => "release",
    };
    format!(
        "{members}, {}, {}, {profile}",
        output::count(count(TargetClass::Lib), TargetClass::Lib.label()),
        output::count(count(TargetClass::Bin), TargetClass::Bin.label()),
    )
}

#[cfg(test)]
mod member_check_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn s(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    fn skip(package: &str) -> MemberSkip {
        MemberSkip { package: package.into(), reason: "why".into() }
    }

    #[test]
    fn admission_drops_skipped_members_and_names_every_refused_cli_package() {
        let members = s(&["a", "b", "c"]);
        let (kept, notes) = admit_members(&members, &[skip("b")], &[]).unwrap();
        assert_eq!(kept, s(&["a", "c"]));
        assert!(notes.is_empty());

        let (kept, notes) = admit_members(&members, &[skip("b")], &s(&["c", "b", "x", "c"])).unwrap();
        assert_eq!(kept, s(&["c"]));
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("skipped") && notes[1].contains("not a workspace member"));
    }

    // A narrowed invocation must not hide a stale skip entry.
    #[test]
    fn a_stale_skip_fails_even_under_a_narrowing_cli() {
        let members = s(&["a"]);
        assert!(admit_members(&members, &[skip("gone")], &s(&["a"])).is_err());
    }

    // `--bins` silently skips a bin whose `required-features` are
    // unsatisfied, so "the invocation succeeded" is not "the targets were
    // checked" - the artifact stream is the proof.
    #[test]
    fn only_the_members_own_non_test_artifacts_credit_a_target() {
        let id = "path+file:///x/daemon#daemon-pkg@0.1.0";
        let stream = concat!(
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/daemon#daemon-pkg@0.1.0","target":{"name":"ba-daemon","kind":["bin"]},"profile":{"test":false}}"#,
            "\n",
            // A test-harness compile of the lib does not credit the lib.
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/daemon#daemon-pkg@0.1.0","target":{"name":"daemon","kind":["lib"]},"profile":{"test":true}}"#,
            "\n",
            // A sibling's bin of the same name does not credit this member.
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/other#other@0.1.0","target":{"name":"ba-worker","kind":["bin"]},"profile":{"test":false}}"#,
            "\n",
            // Proc-macro is a library kind.
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/daemon#daemon-pkg@0.1.0","target":{"name":"daemon_macros","kind":["proc-macro"]},"profile":{"test":false}}"#,
            "\n",
        );
        let scan = scan_artifacts(stream, id);
        let expected: std::collections::BTreeSet<_> = [
            (TargetClass::Bin, "ba-daemon".to_owned()),
            (TargetClass::Lib, "daemon_macros".to_owned()),
        ]
        .into_iter()
        .collect();
        assert_eq!(scan.checked, expected);
    }

    // THE TOOLCHAIN REFUSAL ABORTS THE WHOLE PHASE, so it must be a
    // positive identification. The first version tested for the substring
    // "nightly", which ordinary compile output satisfies constantly.
    #[test]
    fn only_cargos_own_option_rejection_reads_as_an_unsupported_toolchain() {
        assert!(is_feature_unification_refusal(
            "error: the `-Z` flag is only accepted on the nightly channel of Cargo",
            false
        ));
        assert!(is_feature_unification_refusal("error: unknown `-Z` flag specified", false));
        assert!(!is_feature_unification_refusal(
            "error: failed to select a version for `nightly-helper`",
            false
        ));
        assert!(!is_feature_unification_refusal(
            "error[E0658]: use of unstable library feature; add `#![feature(x)]` \
             to the crate attributes to enable (nightly only)",
            true
        ));
        assert!(!is_feature_unification_refusal(
            "error: unknown `-Z` flag specified",
            true
        ));
    }

    // One crate version under two feature sets in a single resolve is named,
    // without claiming it caused anything.
    #[test]
    fn several_feature_sets_of_one_dependency_lib_are_named() {
        let stream = concat!(
            r#"{"reason":"compiler-artifact","package_id":"registry+https://x#nautilus-model@0.62.0","target":{"name":"nautilus-model","kind":["lib"]},"features":["arrow"]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"registry+https://x#nautilus-model@0.62.0","target":{"name":"nautilus-model","kind":["lib"]},"features":[]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"registry+https://x#serde@1.0.0","target":{"name":"serde","kind":["lib"]},"features":["std"]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"registry+https://x#serde@1.0.0","target":{"name":"serde","kind":["lib"]},"features":["std"]}"#,
            "\n",
        );
        let scan = scan_artifacts(stream, "whatever");
        assert_eq!(scan.multi_variant, vec!["nautilus-model".to_owned()]);
    }

    #[test]
    fn the_ok_line_counts_classes_and_names_the_profile() {
        let plan = |name: &str, expected: Vec<(TargetClass, String)>| MemberPlan {
            name: name.into(),
            id: name.into(),
            expected,
        };
        let plans = vec![
            plan("a", vec![(TargetClass::Lib, "a".into()), (TargetClass::Bin, "a".into())]),
            plan("b", vec![(TargetClass::Lib, "b".into())]),
        ];
        assert_eq!(member_counts(&plans, 2, MemberProfile::Dev), "2 members, 2 libs, 1 bin, dev");
        assert_eq!(
            member_counts(&plans, 3, MemberProfile::Release),
            "2 of 3 members, 2 libs, 1 bin, release"
        );
    }

    #[test]
    fn selectors_follow_the_classes_a_member_has() {
        let plan = MemberPlan {
            name: "a".into(),
            id: "a".into(),
            expected: vec![(TargetClass::Bin, "x".into())],
        };
        assert_eq!(plan.selectors(), vec!["--bins".to_owned()]);
    }
}
