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
// full cargo launch envelope - see direct_runtime.rs; the sequential
// and process-isolated lanes still execute through cargo.

/// One test executable and its owning package, from the build's artifact
/// stream.
#[derive(Debug, Clone)]
struct TestBinary {
    package: String,
    /// The full cargo package id - the key the runtime index is filed under,
    /// because package *names* are ambiguous across sources and versions.
    package_id: String,
    /// Target name (`--test <target>` filterable for integration tests).
    target: String,
    /// `"test"` for integration targets, `"lib"`/`"bin"` for unit-test
    /// harnesses.
    kind: String,
    executable: String,
    /// The owning package's root (its `Cargo.toml`'s directory). Cargo runs
    /// test binaries with this as cwd, so direct execution and listing must
    /// too - a fixture test doing `Path::new("tests/fixtures/x")` passes
    /// under cargo and fails from the workspace root.
    manifest_dir: PathBuf,
}

/// What one package's build script contributed, from the prebuild's
/// `build-script-executed` messages. Cargo supplies all three to the test
/// processes it launches; direct execution reconstructs them from here.
#[derive(Debug, Clone, Default)]
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
struct BuildRuntimeIndex {
    build_scripts: std::collections::HashMap<String, BuildScriptOut>,
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
        self.build_scripts.extend(other.build_scripts);
        for (k, v) in other.bin_exes {
            let slot = self.bin_exes.entry(k).or_default();
            for pair in v {
                if !slot.contains(&pair) {
                    slot.push(pair);
                }
            }
        }
    }

    /// Every `rustc-link-search` directory any build script emitted, in
    /// stream order - the loader-path head, matching what cargo adds when it
    /// runs test binaries itself.
    fn all_linked_paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        for bs in self.build_scripts.values() {
            for p in &bs.linked_paths {
                if !out.contains(p) {
                    out.push(p.clone());
                }
            }
        }
        out
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
/// The parallel lane needs both; the listing-only callers drop the index.
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

    if commands {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }
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
    Ok(Some(parse_test_binaries(&String::from_utf8_lossy(
        &captured.stdout,
    ))))
}

/// Parse the artifact stream: test-profile executables become
/// [`TestBinary`]s, while `build-script-executed` messages and non-test bin
/// executables land in the [`BuildRuntimeIndex`] the direct-execution lane
/// reconstructs cargo's runtime env from.
fn parse_test_binaries(stdout: &str) -> (Vec<TestBinary>, BuildRuntimeIndex) {
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
    for line in stdout.lines() {
        let Ok(a) = serde_json::from_str::<Artifact>(line) else {
            continue;
        };

        if a.reason == "build-script-executed" {
            let slot = index.build_scripts.entry(a.package_id).or_default();
            slot.out_dir = a.out_dir;
            slot.env = a.env;
            // `linked_paths` entries may carry a `KIND=` prefix
            // (`native=/path`); the loader path wants the bare directory.
            slot.linked_paths = a
                .linked_paths
                .iter()
                .map(|p| p.split_once('=').map_or(p.as_str(), |(_, path)| path).to_owned())
                .collect();
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
    (out, index)
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
/// which stays cargo's job; listing executes no test code, so loading is
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
/// Listing executes no test code, so direct execution is env-safe once
/// the loader path is supplied (see [`loader_path`]). `Ok(None)` means
/// the listing failed and was already reported.
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
    let captured = output::run_captured_with_env(&binary.executable, &args, cwd, &env)?;

    if !captured.status.success() {
        output::error(&format!(
            "failing command: {} {}",
            binary.executable,
            args.join(" ")
        ));
        output::error(&String::from_utf8_lossy(&captured.stderr));
        return Ok(None);
    }
    Ok(Some(parse_list_output(&String::from_utf8_lossy(
        &captured.stdout,
    ))))
}

/// Restrict the binary set to a lane's `--test <target>` filters: cargo
/// semantics, where any `--test` flag selects only the named integration
/// targets and drops lib/bin unit tests.
fn filter_binaries<'a>(
    binaries: &'a [TestBinary],
    cargo_test_filters: &[String],
) -> Vec<&'a TestBinary> {
    let targets: Vec<&str> = cargo_test_filters
        .iter()
        .filter(|a| *a != "--test")
        .map(String::as_str)
        .collect();

    if targets.is_empty() {
        return binaries.iter().collect();
    }
    binaries
        .iter()
        .filter(|b| b.kind == "test" && targets.contains(&b.target.as_str()))
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
        let (bins, _) = parse_test_binaries(stdout);
        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0].package, "pkg-a");
        assert_eq!(bins[0].kind, "lib");
        assert_eq!(bins[0].manifest_dir, std::path::Path::new("/x/a"));
        assert_eq!(bins[1].package, "pkg-b");
        assert_eq!(bins[1].target, "serial_tests");
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
        let (bins, index) = parse_test_binaries(stdout);
        assert!(bins.is_empty());
        let bs = index.build_scripts.get("path+file:///x/a#pkg-a@0.1.0").unwrap();
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
}
