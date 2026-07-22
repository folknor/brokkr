// Per-binary test attribution (TIERED-CHECK.md feature 11).
//
// Package-qualified skips and package-qualified coverage pairs need to
// know which package owns each test. Cargo-mediated listing cannot
// provide that: cargo prints per-binary attribution on stderr while the
// listing arrives on stdout - separately captured streams with no
// reliable correlation. Instead, `cargo test --no-run
// --message-format=json` yields every test executable with its owning
// package, and each binary then runs `--list` *directly* - safe because
// listing executes no test code, so the cargo-env argument against
// direct execution (CARGO_MANIFEST_DIR, OUT_DIR, …) does not apply to
// enumeration. Execution still goes through cargo.

/// One test executable and its owning package, from the build's artifact
/// stream.
#[derive(Debug, Clone)]
struct TestBinary {
    package: String,
    /// Target name (`--test <target>` filterable for integration tests).
    target: String,
    /// `"test"` for integration targets, `"lib"`/`"bin"` for unit-test
    /// harnesses.
    kind: String,
    executable: String,
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
    let mut args: Vec<String> = vec![
        "test".into(),
        "--no-run".into(),
        "--message-format=json".into(),
    ];
    args.extend(selection.iter().cloned());
    args.push("--tests".into());

    if commands {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, env_refs)?;

    if !captured.status.success() {
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        output::error(&String::from_utf8_lossy(&captured.stderr));
        return Ok(None);
    }
    Ok(Some(parse_test_binaries(&String::from_utf8_lossy(
        &captured.stdout,
    ))))
}

/// Parse the artifact stream: keep compiler-artifact messages that carry
/// an executable built under the test profile.
fn parse_test_binaries(stdout: &str) -> Vec<TestBinary> {
    #[derive(serde::Deserialize)]
    struct Artifact {
        reason: String,
        package_id: String,
        target: ArtifactTarget,
        profile: ArtifactProfile,
        executable: Option<String>,
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
    for line in stdout.lines() {
        let Ok(a) = serde_json::from_str::<Artifact>(line) else {
            continue;
        };

        if a.reason != "compiler-artifact" || !a.profile.test {
            continue;
        }
        let Some(exe) = a.executable else { continue };
        out.push(TestBinary {
            package: package_name_from_id(&a.package_id),
            target: a.target.name,
            kind: a.target.kind.first().cloned().unwrap_or_default(),
            executable: exe,
        });
    }
    out
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

/// Run one built test binary with `--list` plus the given libtest args.
/// Listing executes no test code, so direct execution is env-safe.
/// `Ok(None)` means the listing failed and was already reported.
fn binary_list(
    binary: &TestBinary,
    project_root: &Path,
    libtest_args: &[&str],
    env_refs: &[(&str, &str)],
) -> Result<Option<Vec<String>>, DevError> {
    let mut args: Vec<&str> = libtest_args.to_vec();
    args.push("--list");
    let captured =
        output::run_captured_with_env(&binary.executable, &args, project_root, env_refs)?;

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
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/a#pkg-a@0.1.0","target":{"name":"pkg-a","kind":["lib"]},"profile":{"test":true},"executable":"/t/deps/pkg_a-1"}"#,
            "\n",
            // Non-test profile (the normal lib build): dropped.
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/a#pkg-a@0.1.0","target":{"name":"pkg-a","kind":["lib"]},"profile":{"test":false},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x/b#pkg-b@0.1.0","target":{"name":"serial_tests","kind":["test"]},"profile":{"test":true},"executable":"/t/deps/serial_tests-2"}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
        );
        let bins = parse_test_binaries(stdout);
        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0].package, "pkg-a");
        assert_eq!(bins[0].kind, "lib");
        assert_eq!(bins[1].package, "pkg-b");
        assert_eq!(bins[1].target, "serial_tests");
    }

    #[test]
    fn target_filters_follow_cargo_semantics() {
        let bins = vec![
            TestBinary {
                package: "a".into(),
                target: "a".into(),
                kind: "lib".into(),
                executable: "/1".into(),
            },
            TestBinary {
                package: "a".into(),
                target: "cli_sort".into(),
                kind: "test".into(),
                executable: "/2".into(),
            },
        ];
        // No filter: everything.
        assert_eq!(filter_binaries(&bins, &[]).len(), 2);
        // `--test cli_sort`: only the named integration target; the lib
        // unit tests are dropped, mirroring cargo.
        let filtered = filter_binaries(&bins, &["--test".into(), "cli_sort".into()]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].target, "cli_sort");
    }
}
