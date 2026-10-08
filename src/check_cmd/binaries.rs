// Per-binary test attribution: which package owns each test.
//
// Package-qualified skips and package-qualified coverage pairs need to
// know which package owns each test. Cargo-mediated listing cannot
// provide that: cargo prints per-binary attribution on stderr while the
// listing arrives on stdout - separately captured streams with no
// reliable correlation. Instead, `cargo test --no-run
// --message-format=json` yields every test executable with its owning
// package, and each binary then runs `--list` *directly* (from the
// owning package's root, since a custom harness can observe cwd while
// listing). The parallel lane's *execution* is direct too, under the
// full cargo launch envelope - see direct_runtime.rs - and so is the
// process-isolated lane's; the sequential lane still executes through cargo.

/// One test executable and its owning package, from the build's artifact
/// stream.
#[derive(Debug, Clone)]
pub(crate) struct TestBinary {
    pub(crate) package: String,
    /// The full cargo package id - the key the runtime index is filed under,
    /// because package *names* are ambiguous across sources and versions.
    package_id: String,
    /// Target name (`--test <target>` filterable for integration tests).
    pub(crate) target: String,
    /// `"test"` for integration targets, `"lib"`/`"bin"` for unit-test
    /// harnesses.
    kind: String,
    pub(crate) executable: String,
    /// The owning package's root (its `Cargo.toml`'s directory). Cargo runs
    /// test binaries with this as cwd, so direct execution and listing must
    /// too - a fixture test doing `Path::new("tests/fixtures/x")` passes
    /// under cargo and fails from the workspace root.
    pub(crate) manifest_dir: PathBuf,
}

/// A `TestBinary` for other modules' unit tests, which cannot name the
/// private fields.
#[cfg(test)]
pub(crate) fn test_binary_for_tests(package: &str, kind: &str, target: &str) -> TestBinary {
    TestBinary {
        package: package.into(),
        package_id: format!("path+file:///x/{package}#{package}@0.1.0"),
        target: target.into(),
        kind: kind.into(),
        executable: format!("/t/debug/deps/{target}-1"),
        manifest_dir: PathBuf::from(format!("/x/{package}")),
    }
}

impl TestBinary {
    /// The target kind cargo's selectors answer to: `lib` for every library
    /// flavour (`rlib`, `proc-macro`, ...), else the kind itself.
    pub(crate) fn selector_kind(&self) -> &str {
        selector_kind_of(&self.kind)
    }

    /// `kind:target`, how a failure names the harness it came from.
    pub(crate) fn label(&self) -> String {
        format!("{}:{}", self.selector_kind(), self.target)
    }
}

/// What one package's build script contributed, from the prebuild's
/// `build-script-executed` messages. Cargo supplies all three to the test
/// processes it launches; direct execution reconstructs them from here.
///
/// `out_dir` is kept for identity only - it is what tells two runs of one
/// package's build script apart - and is never exported: cargo sets `OUT_DIR`
/// for build scripts and rustdoc, not for the test processes it launches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BuildScriptOut {
    out_dir: Option<String>,
    /// `cargo::rustc-env=K=V` pairs, exported to the test process.
    env: Vec<(String, String)>,
    /// `cargo::rustc-link-search` directories, folded into the loader path.
    linked_paths: Vec<String>,
}

/// Everything the artifact stream carries beyond the test binaries
/// themselves, keyed by full package id: build-script output and the
/// non-test bin executables that back runtime `CARGO_BIN_EXE_<name>` reads.
#[derive(Debug, Clone, Default)]
pub(crate) struct BuildRuntimeIndex {
    /// Every distinct run of each package's build script. Usually one; a
    /// package built twice in one stream (for the host and for the target, or
    /// under two feature sets) has one per build, and the stream does not say
    /// which one a test executable linked against - so the envelope uses them
    /// only where they agree, rather than letting the last one silently win.
    build_scripts: std::collections::HashMap<String, Vec<BuildScriptOut>>,
    /// package id -> (bin target name, executable path). Cargo 1.94+ exposes
    /// `CARGO_BIN_EXE_<name>` to test processes at *runtime*, not only via
    /// `env!`, so the fan-out must be able to reproduce it.
    bin_exes: std::collections::HashMap<String, Vec<(String, String)>>,
}

impl BuildRuntimeIndex {
    /// Fold another prebuild's stream in (package mode runs one prebuild per
    /// package; their indexes union, and a duplicate key carries the same
    /// facts, so last-wins is harmless).
    fn merge(&mut self, other: BuildRuntimeIndex) {
        for (k, runs) in other.build_scripts {
            let slot = self.build_scripts.entry(k).or_default();
            for run in runs {
                if !slot.contains(&run) {
                    slot.push(run);
                }
            }
        }
        for (k, v) in other.bin_exes {
            let slot = self.bin_exes.entry(k).or_default();
            for pair in v {
                if !slot.contains(&pair) {
                    slot.push(pair);
                }
            }
        }
    }

    /// Every `rustc-link-search` directory any build script emitted, sorted
    /// and deduplicated - cargo keeps these in a `BTreeSet`, so its loader
    /// path is ordered by path, not by which build script ran first.
    fn all_linked_paths(&self) -> Vec<String> {
        let set: std::collections::BTreeSet<&String> = self
            .build_scripts
            .values()
            .flatten()
            .flat_map(|bs| bs.linked_paths.iter())
            .collect();
        set.into_iter().cloned().collect()
    }
}

/// Build (or no-op re-check) the selection's test binaries and return
/// them with package attribution. `Ok(None)` means the build failed and
/// was already reported.
fn test_binaries(
    project_root: &Path,
    selection: &[String],
    env_refs: &[(&str, &str)],
    commands: bool,
) -> Result<Option<Vec<TestBinary>>, DevError> {
    Ok(test_binaries_with_runtime(project_root, selection, env_refs, commands)?
        .map(|(bins, _)| bins))
}

/// [`test_binaries`] plus the [`BuildRuntimeIndex`] the same stream carries.
/// Both direct-execution lanes need it; listing-only callers drop the index.
fn test_binaries_with_runtime(
    project_root: &Path,
    selection: &[String],
    env_refs: &[(&str, &str)],
    commands: bool,
) -> Result<Option<(Vec<TestBinary>, BuildRuntimeIndex)>, DevError> {
    let mut args: Vec<String> = vec![
        "test".into(),
        "--no-run".into(),
        "--message-format=json".into(),
    ];
    args.extend(selection.iter().cloned());
    // `--tests` builds lib+bins+integration harnesses. A selection that
    // already names a target (`--test <name>`) must not be broadened -
    // cargo unions selection flags, so `--test foo --tests` would build
    // every harness and enumerate tests the lane meant to exclude.
    if !has_target_selector(&args) {
        args.push("--tests".into());
    }

    cargo_line(commands, &format!("cargo {}", args.join(" ")));
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, env_refs)?;

    if !captured.status.success() {
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        let stderr = String::from_utf8_lossy(&captured.stderr);
        output::error(&stderr);
        // A deliberate hard stop, not a fallback: running the fan-out
        // without the unification pin recreates the mismatched-graph run it
        // exists to prevent (see parallel.rs's module header). Scoped to
        // rejections that name the flag so an ordinary compile error is not
        // decorated with an irrelevant remedy.
        if selection.iter().any(|a| a == "-Zfeature-unification")
            && (stderr.contains("feature-unification") || stderr.contains("nightly"))
        {
            output::error(
                "this cargo does not support -Zfeature-unification, which the \
                 parallel test lane needs to keep its per-binary runs on the \
                 prebuild's feature graph. Update the nightly toolchain, or \
                 drop `parallel` from the [[check]] entry.",
            );
        }
        return Ok(None);
    }
    let (binaries, index, recognised) =
        parse_test_binaries(&String::from_utf8_lossy(&captured.stdout));
    // Cargo exited 0 but said nothing this parser recognised. Handing back an
    // empty set would make "no artifact facts were parsed" indistinguishable
    // from "this selection has no test binaries", and the coverage audit
    // certifies over that set - so an unparsed stream became an empty universe
    // and a green audit attesting to nothing.
    if !recognised {
        output::error(&format!(
            "cargo exited successfully but produced no recognisable artifact stream \
             (no build-finished record) for: cargo {}. brokkr cannot tell an empty \
             selection from an unparsed one, and the coverage audit certifies over \
             this set, so this is a hard stop rather than an empty universe.",
            args.join(" ")
        ));
        return Ok(None);
    }
    Ok(Some((binaries, index)))
}

/// Parse the artifact stream: test-profile executables become
/// [`TestBinary`]s, while `build-script-executed` messages and non-test bin
/// executables land in the [`BuildRuntimeIndex`] the direct-execution lane
/// reconstructs cargo's runtime env from.
/// Returns the binaries, the runtime index, and whether the stream was
/// recognisably cargo's (see `saw_build_finished` inside).
fn parse_test_binaries(stdout: &str) -> (Vec<TestBinary>, BuildRuntimeIndex, bool) {
    #[derive(serde::Deserialize)]
    struct Artifact {
        reason: String,
        package_id: String,
        #[serde(default)]
        manifest_path: Option<String>,
        #[serde(default)]
        target: Option<ArtifactTarget>,
        #[serde(default)]
        profile: Option<ArtifactProfile>,
        #[serde(default)]
        executable: Option<String>,
        // build-script-executed fields.
        #[serde(default)]
        out_dir: Option<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default)]
        linked_paths: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct ArtifactTarget {
        name: String,
        kind: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct ArtifactProfile {
        test: bool,
    }

    let mut out = Vec::new();
    let mut index = BuildRuntimeIndex::default();
    // Whether the stream was recognisably cargo's at all. Cargo closes a
    // `--message-format=json` run with `{"reason":"build-finished","success":...}`,
    // so its absence means these were not cargo's artifact facts - and an empty
    // `out` then means "nothing was parsed" rather than "no test binaries exist".
    // The distinction matters because the coverage audit certifies over `out`: an
    // unparsed stream produced an empty universe and a green `0 pairs, 0 orphaned`
    // audit, which attests to nothing while looking like proof.
    let mut saw_build_finished = false;
    #[derive(serde::Deserialize)]
    struct AnyRecord {
        reason: String,
    }
    for line in stdout.lines() {
        // `build-finished` carries no `package_id`, so the strict `Artifact`
        // parse below rejects it. Sniff the reason first.
        if let Ok(r) = serde_json::from_str::<AnyRecord>(line)
            && r.reason == "build-finished"
        {
            saw_build_finished = true;
            continue;
        }
        let Ok(a) = serde_json::from_str::<Artifact>(line) else {
            continue;
        };

        if a.reason == "build-script-executed" {
            let run = BuildScriptOut {
                out_dir: a.out_dir,
                env: a.env,
                // `linked_paths` entries may carry a `KIND=` prefix
                // (`native=/path`); the loader path wants the bare directory.
                linked_paths: a
                    .linked_paths
                    .iter()
                    .map(|p| p.split_once('=').map_or(p.as_str(), |(_, path)| path).to_owned())
                    .collect(),
            };
            let slot = index.build_scripts.entry(a.package_id).or_default();
            if !slot.contains(&run) {
                slot.push(run);
            }
            continue;
        }
        if a.reason != "compiler-artifact" {
            continue;
        }
        let (Some(target), Some(profile)) = (a.target, a.profile) else {
            continue;
        };
        let Some(exe) = a.executable else { continue };
        let kind = target.kind.first().cloned().unwrap_or_default();
        if !profile.test {
            // The runnable bin behind runtime `CARGO_BIN_EXE_<name>` reads.
            if kind == "bin" {
                index
                    .bin_exes
                    .entry(a.package_id)
                    .or_default()
                    .push((target.name, exe));
            }
            continue;
        }
        out.push(TestBinary {
            package: package_name_from_id(&a.package_id),
            package_id: a.package_id,
            target: target.name,
            kind,
            executable: exe,
            manifest_dir: a
                .manifest_path
                .as_deref()
                .map(Path::new)
                .and_then(Path::parent)
                .map_or_else(PathBuf::new, Path::to_path_buf),
        });
    }
    (out, index, saw_build_finished)
}

/// The unit kind a harness answers to under cargo's selectors: its own kind for
/// the four named kinds, and `lib` for everything else (`rlib`, `proc-macro`,
/// `cdylib`, ... are all the library harness).
fn selector_kind_of(kind: &str) -> &str {
    match kind {
        k @ ("test" | "bin" | "example" | "bench") => k,
        _ => "lib",
    }
}

/// The test harnesses in a `cargo test --no-run --message-format=json`
/// artifact stream, in the order cargo runs them (by target kind - lib, bin,
/// test, example, bench - then by name), plus the runtime index the same
/// stream carries for executing them directly. For a one-package selection
/// these are all that package's - dependencies never reach the stream as
/// test-profile executables. `None` when the stream was not recognisably
/// cargo's - see [`parse_test_binaries`] for why that is not the same answer
/// as "no harnesses".
pub(crate) fn prebuilt_harnesses(stdout: &str) -> Option<(Vec<TestBinary>, BuildRuntimeIndex)> {
    let (mut binaries, index, recognised) = parse_test_binaries(stdout);
    if !recognised {
        return None;
    }
    let rank = |b: &TestBinary| match b.selector_kind() {
        "lib" => 0,
        "bin" => 1,
        "test" => 2,
        "example" => 3,
        _ => 4,
    };
    binaries.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.target.cmp(&b.target)));
    binaries.dedup_by(|a, b| a.executable == b.executable);
    Some((binaries, index))
}

/// Extract the package name from a cargo `package_id`, across the
/// formats cargo has used:
/// - spec URL: `path+file:///…/crates/infrastructure#nautilus-infrastructure@0.1.0`
/// - spec URL, name == dir: `path+file:///…/nautilus-cli#0.1.0`
/// - legacy: `nautilus-common 0.1.0 (path+file:///…)`
fn package_name_from_id(id: &str) -> String {
    if let Some((base, frag)) = id.rsplit_once('#') {
        if let Some((name, _ver)) = frag.rsplit_once('@') {
            return name.to_owned();
        }
        // Fragment is a bare version: the name is the last path segment.
        return base.rsplit('/').next().unwrap_or(base).to_owned();
    }
    id.split_whitespace().next().unwrap_or(id).to_owned()
}

/// The toolchain's target-libdir, for the dynamic-loader path when
/// running test binaries directly: proc-macro test binaries link libstd
/// dynamically (rustc dlopens proc-macro crates; they have no choice),
/// and cargo supplies this path itself when it runs test binaries. Found
/// the hard way on nautilus's only `proc-macro = true` crate. Run in the
/// project root so rustup resolves the same toolchain cargo uses.
fn toolchain_libdir(
    project_root: &Path,
    env_refs: &[(&str, &str)],
) -> Result<String, DevError> {
    let captured = output::run_captured_with_env(
        "rustc",
        &["--print", "target-libdir"],
        project_root,
        env_refs,
    )?;

    if !captured.status.success() {
        return Err(DevError::Build(format!(
            "rustc --print target-libdir failed: {}",
            String::from_utf8_lossy(&captured.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&captured.stdout).trim().to_owned())
}

/// The loader path for one binary: toolchain libdir, the exe's own deps
/// dir and its parent (matching what cargo adds when it runs binaries -
/// the deps dirs track per-shape isolated target dirs for free), then
/// `existing` - whatever `LD_LIBRARY_PATH` the run already carried. Loader
/// path only - this is NOT the test-code env (CARGO_MANIFEST_DIR etc.),
/// which stays cargo's job; listing runs no test bodies, so loading is
/// the whole requirement. `existing` is the sweep's own `LD_LIBRARY_PATH`
/// when it declared one (so a `[[check]] env` shared-object path is
/// honored during listing, as it is for the cargo-mediated build/test),
/// otherwise brokkr's inherited value - resolved by the caller.
///
/// Why it is needed at all: a `proc-macro = true` crate links libstd
/// *dynamically* (rustc dlopens it), so direct-exec `--list` on its test
/// binary dies with `error while loading shared libraries: libstd-….so`.
/// Cargo supplies the loader path when cargo runs the binary; listing
/// directly does not, so brokkr supplies it. Two alternatives were
/// rejected: enumerating through cargo instead, because cargo cannot
/// address a single lib unit-test harness without `-p` and `-p` changes
/// feature unification - it can list a *different build* than the lane
/// actually runs; and skipping proc-macro targets, which is a quiet
/// shrink of the universe the coverage audit certifies over. The smoke
/// workspace carries a proc-macro member as the permanent regression
/// test.
fn loader_path(libdir: &str, executable: &str, existing: Option<&str>) -> String {
    let mut paths: Vec<String> = vec![libdir.to_owned()];

    if let Some(deps) = Path::new(executable).parent() {
        paths.push(deps.display().to_string());

        if let Some(profile_dir) = deps.parent() {
            paths.push(profile_dir.display().to_string());
        }
    }

    if let Some(existing) = existing.filter(|e| !e.is_empty()) {
        paths.push(existing.to_owned());
    }
    paths.join(":")
}

/// Run one built test binary with `--list` plus the given libtest args.
/// A libtest listing executes no test *bodies*, so direct execution is
/// env-safe once the loader path is supplied (see [`loader_path`]). It is not
/// "no code", though: static constructors run before `main`, and a custom
/// harness is arbitrary code that may ignore `--list` altogether - so the
/// listing is bounded like a test, by [`crate::test_runner::TEST_TIMEOUT`],
/// and one that overruns fails enumeration the way a failed listing does.
/// `Ok(None)` means the listing failed and was already reported.
fn binary_list(
    binary: &TestBinary,
    project_root: &Path,
    libtest_args: &[&str],
    env_refs: &[(&str, &str)],
    libdir: &str,
) -> Result<Option<Vec<String>>, DevError> {
    let mut args: Vec<&str> = libtest_args.to_vec();
    args.push("--list");
    // The `LD_LIBRARY_PATH` this run already carries: the sweep's own
    // (from `[[check]] env`) when it set one, else brokkr's inherited
    // value. The loader tail folds it in, and the pushed pair below wins
    // over the env_refs copy - so the sweep's path is honored here too.
    let existing = env_refs
        .iter()
        .find(|(k, _)| *k == "LD_LIBRARY_PATH")
        .map(|(_, v)| (*v).to_owned())
        .or_else(|| std::env::var("LD_LIBRARY_PATH").ok());
    let ld = loader_path(libdir, &binary.executable, existing.as_deref());
    let mut env: Vec<(&str, &str)> = env_refs.to_vec();
    env.push(("LD_LIBRARY_PATH", &ld));
    // cwd is the owning package's root, matching where cargo runs the binary:
    // a custom harness or ctor can observe cwd before producing its list.
    let cwd = if binary.manifest_dir.as_os_str().is_empty() {
        project_root
    } else {
        binary.manifest_dir.as_path()
    };
    // Launched as a test run is (own process group, run token, hold
    // capability), with its output bounded after exit: see `run_listing`.
    let run = crate::test_runner::run_listing(
        &binary.executable,
        &args,
        cwd,
        &env,
        crate::test_runner::TEST_TIMEOUT,
    )?;
    if run.unsettled {
        output::error(&format!(
            "failing command: {} {}",
            binary.executable,
            args.join(" ")
        ));
        output::error(
            "the listing's output did not close after it exited - something it started still \
             holds the pipe - so the listing may be truncated and is not used",
        );
        return Ok(None);
    }
    let captured = run;
    if captured.timed_out {
        output::error(&format!(
            "failing command: {} {}",
            binary.executable,
            args.join(" ")
        ));
        output::error(&format!(
            "the listing ran past {}s and was killed - a libtest `--list` answers at once, so a \
             static constructor or a custom harness is doing work (or hanging) before it lists",
            crate::test_runner::TEST_TIMEOUT.as_secs()
        ));
        return Ok(None);
    }

    if !captured.status.success() {
        output::error(&format!(
            "failing command: {} {}",
            binary.executable,
            args.join(" ")
        ));
        output::error(&String::from_utf8_lossy(&captured.stderr));
        return Ok(None);
    }
    // A listing that is not a libtest listing must fail enumeration rather than
    // contribute an empty set. Silently treating "this binary does not speak
    // --list" as "this binary has no tests" is how a coverage audit certifies a
    // universe it never saw - and a green audit is taken as evidence.
    let Some(names) = parse_list_output(&String::from_utf8_lossy(&captured.stdout)) else {
        output::error(&format!(
            "{} did not produce a complete libtest listing (no `N tests, M benchmarks` tally, or \
             one that disagrees with the entries listed above it). A target \
             built with `harness = false`, or any custom harness that ignores `--list`, cannot be \
             enumerated - so it cannot be audited or run through the isolated lane. Exclude the \
             target from this sweep, or give it a libtest harness.",
            binary.executable
        ));
        return Ok(None);
    };
    Ok(Some(names))
}

/// One cargo target selector, as [`filter_binaries`] evaluates it.
enum TargetSelector {
    /// `--test NAME` / `--bin NAME` / `--example NAME` / `--bench NAME`, or
    /// the `=`-attached spelling: the named targets of that kind. `NAME` may
    /// be a glob, as cargo allows.
    Named { kind: &'static str, name: String },
    /// `--lib`: the library unit-test harness.
    Lib,
    /// `--bins` / `--examples` / `--benches`: every target of that kind.
    Kind(&'static str),
    /// `--tests` / `--all-targets`: everything the enumeration built. `--tests`
    /// depends on per-target `test = true` manifest flags this struct does not
    /// carry, so it is read as "whatever cargo built", which is exact whenever
    /// the enumeration saw the flag.
    Everything,
    /// `--doc` (and the `--doctests` spelling): the doctest pseudo-target,
    /// which has no binary to select.
    Nothing,
}

/// [`selector_kind_of`] for an enumerated binary.
fn selector_kind(binary: &TestBinary) -> &str {
    selector_kind_of(&binary.kind)
}

/// Whether `target` matches a selector's `name`, with cargo's glob support.
/// A name that is not a valid glob is compared literally.
fn target_name_matches(name: &str, target: &str) -> bool {
    globset::Glob::new(name).map_or(name == target, |g| g.compile_matcher().is_match(target))
}

/// Parse a selector list (`["--test", "a", "--lib", "--bin=x"]`). Tokens
/// that are not target selectors are ignored; a value-taking selector with
/// no value selects nothing.
fn parse_target_selectors(args: &[String]) -> Vec<TargetSelector> {
    const VALUED: [(&str, &str); 4] = [
        ("--test", "test"),
        ("--bin", "bin"),
        ("--example", "example"),
        ("--bench", "bench"),
    ];
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let (head, inline) = a.split_once('=').map_or((a, None), |(h, v)| (h, Some(v)));
        if let Some((_, kind)) = VALUED.iter().find(|(flag, _)| *flag == head) {
            let value = match inline {
                Some(v) => Some(v.to_owned()),
                None => {
                    i += 1;
                    args.get(i).cloned()
                }
            };
            if let Some(name) = value {
                out.push(TargetSelector::Named { kind, name });
            }
        } else {
            match a {
                "--lib" => out.push(TargetSelector::Lib),
                "--bins" => out.push(TargetSelector::Kind("bin")),
                "--examples" => out.push(TargetSelector::Kind("example")),
                "--benches" => out.push(TargetSelector::Kind("bench")),
                "--tests" | "--all-targets" => out.push(TargetSelector::Everything),
                "--doc" | "--doctests" => out.push(TargetSelector::Nothing),
                _ => {}
            }
        }
        i += 1;
    }
    out
}

/// Restrict the binary set to a lane's target selectors, with cargo's
/// semantics: selectors UNION (`--lib --test cli` is the lib harness plus
/// `cli`), and any selector at all drops the targets none of them name. An
/// empty list keeps everything.
///
/// Every selector kind is honoured, not only `--test NAME`. The parallel lane
/// feeds the forwarded selectors (`-- --lib`, `-- --test=cli`, `-- --bins`)
/// through here alongside the sweep's own `--test` filters; reading every
/// token after a bare `--test` as a target name made each of those match no
/// binary, and the lane refused as "matched no test binaries".
///
/// Where the enumeration already carried the same selectors (every
/// non-package-mode resolution), this filter is idempotent. Under package
/// mode the sweep's `--test` filters cannot ride the per-package enumeration
/// (cargo refuses a target a package lacks), so this is where they narrow.
fn filter_binaries<'a>(binaries: &'a [TestBinary], selectors: &[String]) -> Vec<&'a TestBinary> {
    let parsed = parse_target_selectors(selectors);
    if parsed.is_empty() {
        return binaries.iter().collect();
    }
    binaries
        .iter()
        .filter(|b| {
            parsed.iter().any(|s| match s {
                TargetSelector::Named { kind, name } => {
                    selector_kind(b) == *kind && target_name_matches(name, &b.target)
                }
                TargetSelector::Lib => selector_kind(b) == "lib",
                TargetSelector::Kind(kind) => selector_kind(b) == *kind,
                TargetSelector::Everything => true,
                TargetSelector::Nothing => false,
            })
        })
        .collect()
}

#[cfg(test)]
mod binaries_tests {
    #![allow(clippy::unwrap_used)]

    use super::{filter_binaries, package_name_from_id, parse_test_binaries, TestBinary};

    #[test]
    fn package_id_formats_all_parse() {
        assert_eq!(
            package_name_from_id(
                "path+file:///home/x/nt/crates/infrastructure#nautilus-infrastructure@0.1.0"
            ),
            "nautilus-infrastructure"
        );
        assert_eq!(
            package_name_from_id("path+file:///home/x/nautilus-cli#0.1.0"),
            "nautilus-cli"
        );
        assert_eq!(
            package_name_from_id("nautilus-common 0.1.0 (path+file:///home/x/nt)"),
            "nautilus-common"
        );
        assert_eq!(
            package_name_from_id(
                "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"
            ),
            "serde"
        );
    }

    #[test]
    fn artifact_stream_keeps_test_profile_executables() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/a#pkg-a@0.1.0","manifest_path":"/x/a/Cargo.toml","target":{"name":"pkg-a","kind":["lib"]},"profile":{"test":true},"executable":"/t/deps/pkg_a-1"}"#,
            "\n",
            // Non-test profile (the normal lib build): dropped.
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/a#pkg-a@0.1.0","manifest_path":"/x/a/Cargo.toml","target":{"name":"pkg-a","kind":["lib"]},"profile":{"test":false},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/b#pkg-b@0.1.0","manifest_path":"/x/b/Cargo.toml","target":{"name":"serial_tests","kind":["test"]},"profile":{"test":true},"executable":"/t/deps/serial_tests-2"}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
        );
        let (bins, _, recognised) = parse_test_binaries(stdout);
        assert!(recognised, "the build-finished record makes this cargo's stream");
        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0].package, "pkg-a");
        assert_eq!(bins[0].kind, "lib");
        assert_eq!(bins[0].manifest_dir, std::path::Path::new("/x/a"));
        assert_eq!(bins[1].package, "pkg-b");
        assert_eq!(bins[1].target, "serial_tests");
    }

    /// An unrecognised stream must be distinguishable from a selection with no
    /// test binaries. Cargo closes a `--message-format=json` run with a
    /// `build-finished` record; without one, an empty result means "nothing was
    /// parsed", and the coverage audit certifies over that set - so an unparsed
    /// stream used to yield an empty universe and a green audit.
    #[test]
    fn a_stream_without_build_finished_is_not_recognised() {
        let (bins, _, recognised) = parse_test_binaries("");
        assert!(bins.is_empty());
        assert!(!recognised, "silence is not cargo's artifact stream");

        let (_, _, recognised) = parse_test_binaries("some other tool's output\n");
        assert!(!recognised);

        // Artifacts but no terminator: a truncated stream, not a complete one.
        let truncated = concat!(
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/a#pkg-a@0.1.0","manifest_path":"/x/a/Cargo.toml","target":{"name":"pkg-a","kind":["lib"]},"profile":{"test":true},"executable":"/t/deps/pkg_a-1"}"#,
            "\n",
        );
        let (bins, _, recognised) = parse_test_binaries(truncated);
        assert_eq!(bins.len(), 1, "the artifact is still parsed");
        assert!(!recognised, "but the stream never said it finished");
    }

    // The runtime index is what direct execution reconstructs cargo's env
    // from: build-script out_dir/rustc-env/link-search per package id, and
    // the non-test bin executables behind runtime CARGO_BIN_EXE reads.
    #[test]
    fn artifact_stream_fills_the_runtime_index() {
        let stdout = concat!(
            r#"{"reason":"build-script-executed","package_id":"path+file:///x/a#pkg-a@0.1.0","out_dir":"/t/build/pkg-a/out","env":[["GENERATED_ENDPOINT","svc"]],"linked_paths":["native=/t/build/pkg-a/out","/plain"]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/a#pkg-a@0.1.0","manifest_path":"/x/a/Cargo.toml","target":{"name":"servebin","kind":["bin"]},"profile":{"test":false},"executable":"/t/debug/servebin"}"#,
            "\n",
        );
        let (bins, index, _) = parse_test_binaries(stdout);
        assert!(bins.is_empty());
        let runs = index.build_scripts.get("path+file:///x/a#pkg-a@0.1.0").unwrap();
        assert_eq!(runs.len(), 1);
        let bs = &runs[0];
        assert_eq!(bs.out_dir.as_deref(), Some("/t/build/pkg-a/out"));
        assert_eq!(bs.env, vec![("GENERATED_ENDPOINT".to_owned(), "svc".to_owned())]);
        // The `native=` prefix is stripped; a bare path passes through.
        assert_eq!(bs.linked_paths, vec!["/t/build/pkg-a/out", "/plain"]);
        assert_eq!(
            index.bin_exes.get("path+file:///x/a#pkg-a@0.1.0").unwrap(),
            &vec![("servebin".to_owned(), "/t/debug/servebin".to_owned())]
        );
    }

    fn bin(package: &str, target: &str, kind: &str, exe: &str) -> TestBinary {
        TestBinary {
            package: package.into(),
            package_id: format!("path+file:///x/{package}#{package}@0.1.0"),
            target: target.into(),
            kind: kind.into(),
            executable: exe.into(),
            manifest_dir: std::path::PathBuf::from(format!("/x/{package}")),
        }
    }

    // `brokkr test` runs the harnesses of its package in cargo's own order -
    // including examples and benches with `test = true`, which a lib-only view
    // would silently miss.
    #[test]
    fn package_harnesses_come_back_in_cargo_order() {
        let art = |pkg: &str, name: &str, kind: &str| {
            format!(
                r#"{{"reason":"compiler-artifact","package_id":"path+file:///x/{pkg}#{pkg}@0.1.0","manifest_path":"/x/{pkg}/Cargo.toml","target":{{"name":"{name}","kind":["{kind}"]}},"profile":{{"test":true}},"executable":"/t/deps/{name}-1"}}"#
            )
        };
        let stdout = [
            art("a", "zeta", "test"),
            art("a", "demo", "example"),
            art("a", "a", "rlib"),
            art("a", "alpha", "test"),
            art("a", "tool", "bin"),
            r#"{"reason":"build-finished","success":true}"#.to_owned(),
        ]
        .join("\n");
        let (harnesses, _) = super::prebuilt_harnesses(&stdout).unwrap();
        let labels: Vec<String> = harnesses.iter().map(TestBinary::label).collect();
        assert_eq!(labels, vec!["lib:a", "bin:tool", "test:alpha", "test:zeta", "example:demo"]);

        assert!(super::prebuilt_harnesses("").is_none(), "silence is not a stream");
    }

    #[test]
    fn target_filters_follow_cargo_semantics() {
        let bins = vec![bin("a", "a", "lib", "/1"), bin("a", "cli_sort", "test", "/2")];
        // No filter: everything.
        assert_eq!(filter_binaries(&bins, &[]).len(), 2);
        // `--test cli_sort`: only the named integration target; the lib
        // unit tests are dropped, mirroring cargo.
        let filtered = filter_binaries(&bins, &["--test".into(), "cli_sort".into()]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].target, "cli_sort");
    }

    // Forwarded selectors other than `--test NAME` used to be read as
    // target names and matched nothing. Every selector kind now selects what
    // cargo would, and selectors union.
    #[test]
    fn every_selector_kind_selects_what_cargo_would() {
        let bins = vec![
            bin("a", "a", "lib", "/1"),
            bin("a", "cli", "test", "/2"),
            bin("a", "tool", "bin", "/3"),
            bin("m", "m", "proc-macro", "/4"),
        ];
        let names = |sel: &[&str]| -> Vec<String> {
            let sel: Vec<String> = sel.iter().map(|s| (*s).to_owned()).collect();
            filter_binaries(&bins, &sel).iter().map(|b| b.target.clone()).collect()
        };
        assert_eq!(names(&["--lib"]), vec!["a", "m"]);
        assert_eq!(names(&["--test=cli"]), vec!["cli"]);
        assert_eq!(names(&["--bin", "tool"]), vec!["tool"]);
        assert_eq!(names(&["--bins"]), vec!["tool"]);
        assert_eq!(names(&["--tests"]).len(), 4);
        assert_eq!(names(&["--test", "cli", "--lib"]), vec!["a", "cli", "m"]);
        assert_eq!(names(&["--test", "c*"]), vec!["cli"]);
        assert!(names(&["--doc"]).is_empty());
    }
}
