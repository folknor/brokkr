// The launch envelope for executing prebuilt test binaries directly.
//
// The parallel lane's fan-out no longer re-enters cargo (see parallel.rs's
// module header for why the cargo re-entry was structurally unsound): after
// the one prebuild, each test binary is executed directly. Cargo does more
// than exec a test binary, though - it launches it with a contract of cwd and
// environment that real tests depend on - and this module is that contract,
// reconstructed from the same sources cargo derives it from:
//
// - cwd = the owning package's root (a fixture test opening
//   `tests/fixtures/x.json` passes under cargo, fails from the workspace root)
// - the dynamic-loader path (build-script link-search dirs, the exe's deps
//   dir, the profile dir, the toolchain libdir, then the inherited value - in
//   that order, because ordering decides which same-named .so loads)
// - `[env]` from the cargo config chain, with cargo's `force` and `relative`
//   semantics
// - `CARGO_PKG_*` / `CARGO_MANIFEST_DIR` / `CARGO_MANIFEST_PATH` from cargo
//   metadata (unset manifest fields are EMPTY strings, not absent vars -
//   cargo's documented behaviour)
// - `OUT_DIR` and `cargo::rustc-env` values from the prebuild's
//   `build-script-executed` messages, keyed by full package id
// - runtime `CARGO_BIN_EXE_<name>` for the package's own bins (cargo 1.94+
//   exposes these to the test process at runtime, not only via `env!`)
// - `CARGO` = the cargo brokkr itself invoked
//
// Cargo-owned values are applied LAST, so a sweep's `[[check]] env` cannot
// forge `CARGO_PKG_NAME` or `OUT_DIR` - matching cargo, where the caller's
// environment does not override what cargo sets per package.
//
// What is deliberately NOT reproduced:
// - `CARGO_TARGET_TMPDIR`: compile-time only (`env!`) per cargo's contract;
//   runtime reads are not part of the documented execution environment.
// - Target runners (`[target.<triple>].runner`): a configured runner means
//   direct execution would silently bypass a wrapper (qemu, wine, valgrind),
//   so the lane REFUSES at resolution time instead - see
//   [`refuse_configured_runner`].

/// One workspace package's manifest facts, the source for the `CARGO_PKG_*`
/// family. Parsed from `cargo metadata --no-deps`.
#[derive(Debug, Clone, Default)]
struct PkgRuntimeMeta {
    name: String,
    version: String,
    authors: Vec<String>,
    description: String,
    homepage: String,
    license: String,
    license_file: String,
    repository: String,
    rust_version: String,
    manifest_path: String,
}

impl PkgRuntimeMeta {
    /// The `CARGO_PKG_*` set exactly as cargo exports it to a test process.
    /// Absent optional fields are exported as EMPTY variables, never omitted.
    fn pkg_env(&self) -> Vec<(String, String)> {
        let (major, minor, patch, pre) = split_semver(&self.version);
        vec![
            ("CARGO_PKG_NAME".into(), self.name.clone()),
            ("CARGO_PKG_VERSION".into(), self.version.clone()),
            ("CARGO_PKG_VERSION_MAJOR".into(), major),
            ("CARGO_PKG_VERSION_MINOR".into(), minor),
            ("CARGO_PKG_VERSION_PATCH".into(), patch),
            ("CARGO_PKG_VERSION_PRE".into(), pre),
            ("CARGO_PKG_AUTHORS".into(), self.authors.join(":")),
            ("CARGO_PKG_DESCRIPTION".into(), self.description.clone()),
            ("CARGO_PKG_HOMEPAGE".into(), self.homepage.clone()),
            ("CARGO_PKG_LICENSE".into(), self.license.clone()),
            ("CARGO_PKG_LICENSE_FILE".into(), self.license_file.clone()),
            ("CARGO_PKG_REPOSITORY".into(), self.repository.clone()),
            ("CARGO_PKG_RUST_VERSION".into(), self.rust_version.clone()),
        ]
    }
}

/// `"1.2.3-beta.1"` -> `("1", "2", "3", "beta.1")`. Missing components come
/// back empty rather than erroring: the value is re-exported, not interpreted.
fn split_semver(version: &str) -> (String, String, String, String) {
    let (core, pre) = version.split_once('-').unwrap_or((version, ""));
    // A build-metadata suffix (`+meta`) belongs to neither core nor pre.
    let pre = pre.split_once('+').map_or(pre, |(p, _)| p);
    let core = core.split_once('+').map_or(core, |(c, _)| c);
    let mut parts = core.split('.');
    let mut next = || parts.next().unwrap_or("").to_owned();
    (next(), next(), next(), pre.to_owned())
}

/// One `[env]` entry from the cargo config chain, with the two modifiers that
/// change its meaning.
#[derive(Debug, Clone)]
struct ConfigEnvEntry {
    key: String,
    value: String,
    /// `force = true`: overrides an inherited environment value; without it
    /// the entry only fills a hole.
    force: bool,
}

/// The assembled per-sweep runtime: everything [`Self::envelope`] needs that
/// does not vary per binary.
#[derive(Debug)]
struct DirectRuntime {
    /// Full package id -> manifest facts, for `CARGO_PKG_*`.
    packages: HashMap<String, PkgRuntimeMeta>,
    index: BuildRuntimeIndex,
    /// Union of every build script's link-search dirs, the loader-path head.
    linked_paths: Vec<String>,
    libdir: String,
    /// What `CARGO` is set to: the cargo this brokkr invocation runs.
    cargo: String,
    /// `[env]` from the config chain, highest-precedence file first.
    config_env: Vec<ConfigEnvEntry>,
}

impl DirectRuntime {
    /// Assemble the runtime for one sweep: cargo metadata for the package
    /// facts, the prebuild's artifact index, and the cargo config chain.
    fn load(
        project_root: &Path,
        env_refs: &[(&str, &str)],
        index: BuildRuntimeIndex,
    ) -> Result<Self, DevError> {
        let linked_paths = index.all_linked_paths();
        Ok(Self {
            packages: workspace_pkg_meta(project_root)?,
            linked_paths,
            index,
            libdir: toolchain_libdir(project_root, env_refs)?,
            cargo: cargo_program(),
            config_env: cargo_config_env(project_root),
        })
    }

    /// The cwd and environment for one test binary, layered onto `env_refs`
    /// (the sweep/project env). Returned env is complete: pass it as the
    /// child's explicit env additions over brokkr's inherited environment.
    fn envelope(
        &self,
        binary: &TestBinary,
        env_refs: &[(&str, &str)],
    ) -> (PathBuf, Vec<(String, String)>) {
        let cwd = if binary.manifest_dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            binary.manifest_dir.clone()
        };

        let mut env: Vec<(String, String)> = env_refs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let has = |env: &[(String, String)], key: &str| {
            env.iter().any(|(k, _)| k == key) || std::env::var_os(key).is_some()
        };

        // Cargo config [env]: force overrides, plain entries only fill holes -
        // cargo's own rule. First occurrence in the chain wins, and the chain
        // is ordered highest-precedence first, so skip a key already applied.
        let mut applied: Vec<&str> = Vec::new();
        for entry in &self.config_env {
            if applied.iter().any(|k| *k == entry.key) {
                continue;
            }
            applied.push(&entry.key);
            if entry.force || !has(&env, &entry.key) {
                env.push((entry.key.clone(), entry.value.clone()));
            }
        }

        // Cargo-owned per-package values, applied last so nothing above can
        // forge them (later entries win when the child env is applied in
        // order).
        env.push(("CARGO".into(), self.cargo.clone()));
        env.push((
            "CARGO_MANIFEST_DIR".into(),
            cwd.display().to_string(),
        ));
        if let Some(meta) = self.packages.get(&binary.package_id) {
            env.push(("CARGO_MANIFEST_PATH".into(), meta.manifest_path.clone()));
            env.extend(meta.pkg_env());
        }
        if let Some(bs) = self.index.build_scripts.get(&binary.package_id) {
            if let Some(out_dir) = &bs.out_dir {
                env.push(("OUT_DIR".into(), out_dir.clone()));
            }
            for (k, v) in &bs.env {
                env.push((k.clone(), v.clone()));
            }
        }
        if let Some(bins) = self.index.bin_exes.get(&binary.package_id) {
            for (name, exe) in bins {
                env.push((format!("CARGO_BIN_EXE_{name}"), exe.clone()));
            }
        }

        // The loader path: linked paths first, then the exe's dirs, the
        // toolchain libdir, and whatever the run already carried - cargo's
        // ordering, which decides which same-named library loads.
        let existing = env
            .iter()
            .rev()
            .find(|(k, _)| k == "LD_LIBRARY_PATH")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var("LD_LIBRARY_PATH").ok());
        let mut paths: Vec<String> = self.linked_paths.clone();
        if let Some(deps) = Path::new(&binary.executable).parent() {
            paths.push(deps.display().to_string());
            if let Some(profile_dir) = deps.parent() {
                paths.push(profile_dir.display().to_string());
            }
        }
        paths.push(self.libdir.clone());
        if let Some(existing) = existing.filter(|e| !e.is_empty()) {
            paths.push(existing);
        }
        env.push(("LD_LIBRARY_PATH".into(), paths.join(":")));

        (cwd, env)
    }
}

/// The cargo executable this brokkr run uses, for the child's `CARGO` var.
/// An inherited `CARGO` (brokkr itself launched under cargo, or a caller's
/// override) wins; otherwise the literal `cargo` brokkr spawns, resolved
/// against PATH so the value is an executable path rather than a bare name.
fn cargo_program() -> String {
    if let Ok(c) = std::env::var("CARGO")
        && !c.trim().is_empty()
    {
        return c;
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("cargo");
            if candidate.is_file() {
                return candidate.display().to_string();
            }
        }
    }
    "cargo".into()
}

/// Every workspace member's manifest facts, keyed by full package id.
fn workspace_pkg_meta(project_root: &Path) -> Result<HashMap<String, PkgRuntimeMeta>, DevError> {
    let captured = output::run_captured(
        "cargo",
        &["metadata", "--no-deps", "--format-version", "1"],
        project_root,
    )?;
    if !captured.status.success() {
        return Err(DevError::Build(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&captured.stderr)
        )));
    }
    let val: serde_json::Value = serde_json::from_slice(&captured.stdout)
        .map_err(|e| DevError::Build(format!("cargo metadata output unparseable: {e}")))?;
    let Some(packages) = val.get("packages").and_then(serde_json::Value::as_array) else {
        return Err(DevError::Build("cargo metadata missing packages".into()));
    };

    let s = |pkg: &serde_json::Value, key: &str| {
        pkg.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let mut out = HashMap::new();
    for pkg in packages {
        let Some(id) = pkg.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let authors = pkg
            .get("authors")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        out.insert(
            id.to_owned(),
            PkgRuntimeMeta {
                name: s(pkg, "name"),
                version: s(pkg, "version"),
                authors,
                description: s(pkg, "description"),
                homepage: s(pkg, "homepage"),
                license: s(pkg, "license"),
                license_file: s(pkg, "license_file"),
                repository: s(pkg, "repository"),
                rust_version: s(pkg, "rust_version"),
                manifest_path: s(pkg, "manifest_path"),
            },
        );
    }
    Ok(out)
}

/// `[env]` entries from every cargo config file the build reads, ordered
/// highest-precedence first (the deepest ancestor config outranks
/// `$CARGO_HOME` - the order [`rustflags::config_paths`] already returns).
///
/// Cargo's `relative = true` resolves the value against the directory
/// CONTAINING the `.cargo` directory the entry was read from.
fn cargo_config_env(project_root: &Path) -> Vec<ConfigEnvEntry> {
    let mut out = Vec::new();
    for path in rustflags::config_paths(project_root) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = text.parse::<toml::Table>() else {
            continue;
        };
        let Some(env) = doc.get("env").and_then(toml::Value::as_table) else {
            continue;
        };
        // <dir>/.cargo/config.toml -> <dir>.
        let anchor = path.parent().and_then(Path::parent);
        for (key, val) in env {
            let entry = match val {
                toml::Value::String(v) => ConfigEnvEntry {
                    key: key.clone(),
                    value: v.clone(),
                    force: false,
                },
                toml::Value::Table(t) => {
                    let Some(v) = t.get("value").and_then(toml::Value::as_str) else {
                        continue;
                    };
                    let relative = t
                        .get("relative")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false);
                    let value = match (relative, anchor) {
                        (true, Some(dir)) => dir.join(v).display().to_string(),
                        _ => v.to_owned(),
                    };
                    ConfigEnvEntry {
                        key: key.clone(),
                        value,
                        force: t
                            .get("force")
                            .and_then(toml::Value::as_bool)
                            .unwrap_or(false),
                    }
                }
                _ => continue,
            };
            out.push(entry);
        }
    }
    out
}

/// Refuse the direct-execution lane when a target runner is configured.
///
/// Cargo may run test binaries through a wrapper (`[target.<triple>].runner`
/// or `CARGO_TARGET_<TRIPLE>_RUNNER`) - qemu, wine, valgrind, a privilege
/// wrapper. Executing the binary directly would silently bypass it, which is
/// wrong in a way no output would reveal. Resolution-time rather than
/// config-load-time, because an effective runner depends on the discovered
/// config chain, not on `brokkr.toml`.
///
/// A `[target.'cfg(...)']` selector this evaluator cannot decide counts as
/// configured: the destructive direction is bypassing a real runner, so
/// unknown fails closed (unlike the rustflags evaluator, whose inert
/// direction is the opposite).
fn refuse_configured_runner(project_root: &Path) -> Result<(), DevError> {
    let triple = rustflags::host_triple();
    if let Some(t) = &triple {
        let var = format!(
            "CARGO_TARGET_{}_RUNNER",
            t.to_uppercase().replace('-', "_")
        );
        if std::env::var_os(&var).is_some() {
            return Err(DevError::Config(format!(
                "{var} is set: cargo would run test binaries through that runner, and the \
                 parallel lane's direct execution would bypass it. Unset it, or drop \
                 `parallel` from the [[check]] entry."
            )));
        }
    }
    for path in rustflags::config_paths(project_root) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = text.parse::<toml::Table>() else {
            continue;
        };
        let Some(targets) = doc.get("target").and_then(toml::Value::as_table) else {
            continue;
        };
        for (selector, table) in targets {
            let Some(table) = table.as_table() else { continue };
            if !table.contains_key("runner") {
                continue;
            }
            let applies = if let Some(expr) = selector
                .strip_prefix("cfg(")
                .and_then(|s| s.strip_suffix(')'))
            {
                // Unknown -> Some(true): fail closed, see above.
                rustflags::eval_cfg(expr) != Some(false)
            } else {
                triple.as_deref() == Some(selector.as_str())
            };
            if applies {
                return Err(DevError::Config(format!(
                    "{} configures a runner for target `{}`: cargo would run test binaries \
                     through it, and the parallel lane's direct execution would bypass it. \
                     Remove the runner for this host, or drop `parallel` from the [[check]] \
                     entry.",
                    path.display(),
                    selector
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod direct_runtime_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn semver_splits_including_pre_and_build_metadata() {
        assert_eq!(
            split_semver("1.2.3"),
            ("1".into(), "2".into(), "3".into(), String::new())
        );
        assert_eq!(
            split_semver("0.10.0-beta.1"),
            ("0".into(), "10".into(), "0".into(), "beta.1".into())
        );
        assert_eq!(
            split_semver("1.2.3-rc.1+build.5"),
            ("1".into(), "2".into(), "3".into(), "rc.1".into())
        );
    }

    // Cargo exports absent manifest fields as EMPTY variables, never omits
    // them - a runtime `std::env::var("CARGO_PKG_DESCRIPTION")` on a bare
    // manifest yields Ok("") under cargo, so it must here too.
    #[test]
    fn absent_manifest_fields_export_as_empty_not_missing() {
        let meta = PkgRuntimeMeta {
            name: "pkg".into(),
            version: "0.1.0".into(),
            ..Default::default()
        };
        let env = meta.pkg_env();
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("CARGO_PKG_DESCRIPTION"), Some(""));
        assert_eq!(get("CARGO_PKG_LICENSE_FILE"), Some(""));
        assert_eq!(get("CARGO_PKG_AUTHORS"), Some(""));
        assert_eq!(get("CARGO_PKG_NAME"), Some("pkg"));
    }

    fn runtime_with(index: BuildRuntimeIndex, packages: HashMap<String, PkgRuntimeMeta>) -> DirectRuntime {
        let linked_paths = index.all_linked_paths();
        DirectRuntime {
            packages,
            linked_paths,
            index,
            libdir: "/toolchain/lib".into(),
            cargo: "/usr/bin/cargo".into(),
            config_env: Vec::new(),
        }
    }

    fn test_binary() -> TestBinary {
        TestBinary {
            package: "pkg-a".into(),
            package_id: "path+file:///x/a#pkg-a@0.1.0".into(),
            target: "cli_sort".into(),
            kind: "test".into(),
            executable: "/t/debug/deps/cli_sort-1".into(),
            manifest_dir: PathBuf::from("/x/a"),
        }
    }

    // The launch contract in one assertion set: cwd is the package root, and
    // the cargo-owned values land with the loader path ordered linked-paths
    // first and libdir before the inherited tail.
    #[test]
    fn envelope_reconstructs_cargos_launch_contract() {
        let mut index = BuildRuntimeIndex::default();
        index.build_scripts.insert(
            "path+file:///x/a#pkg-a@0.1.0".into(),
            BuildScriptOut {
                out_dir: Some("/t/build/a/out".into()),
                env: vec![("GENERATED_ENDPOINT".into(), "svc".into())],
                linked_paths: vec!["/t/build/a/out".into()],
            },
        );
        index
            .bin_exes
            .entry("path+file:///x/a#pkg-a@0.1.0".into())
            .or_default()
            .push(("serve-bin".into(), "/t/debug/serve-bin".into()));
        let mut packages = HashMap::new();
        packages.insert(
            "path+file:///x/a#pkg-a@0.1.0".into(),
            PkgRuntimeMeta {
                name: "pkg-a".into(),
                version: "0.1.0".into(),
                manifest_path: "/x/a/Cargo.toml".into(),
                ..Default::default()
            },
        );
        let rt = runtime_with(index, packages);

        let (cwd, env) = rt.envelope(&test_binary(), &[("BROKKR_TEST_BIN_DIR", "/t/debug")]);
        assert_eq!(cwd, PathBuf::from("/x/a"));
        let get = |k: &str| {
            env.iter()
                .rev()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("BROKKR_TEST_BIN_DIR"), Some("/t/debug"));
        assert_eq!(get("CARGO"), Some("/usr/bin/cargo"));
        assert_eq!(get("CARGO_MANIFEST_DIR"), Some("/x/a"));
        assert_eq!(get("CARGO_MANIFEST_PATH"), Some("/x/a/Cargo.toml"));
        assert_eq!(get("CARGO_PKG_NAME"), Some("pkg-a"));
        assert_eq!(get("OUT_DIR"), Some("/t/build/a/out"));
        assert_eq!(get("GENERATED_ENDPOINT"), Some("svc"));
        assert_eq!(get("CARGO_BIN_EXE_serve-bin"), Some("/t/debug/serve-bin"));
        let ld = get("LD_LIBRARY_PATH").unwrap();
        let parts: Vec<&str> = ld.split(':').collect();
        assert_eq!(
            &parts[..4],
            &["/t/build/a/out", "/t/debug/deps", "/t/debug", "/toolchain/lib"]
        );
    }

    // A sweep's `[[check]] env` must not forge cargo-owned values: cargo
    // applies its per-package env over the caller's, so brokkr does too.
    #[test]
    fn sweep_env_cannot_forge_cargo_owned_values() {
        let mut packages = HashMap::new();
        packages.insert(
            "path+file:///x/a#pkg-a@0.1.0".into(),
            PkgRuntimeMeta {
                name: "pkg-a".into(),
                version: "0.1.0".into(),
                manifest_path: "/x/a/Cargo.toml".into(),
                ..Default::default()
            },
        );
        let rt = runtime_with(BuildRuntimeIndex::default(), packages);
        let (_, env) = rt.envelope(&test_binary(), &[("CARGO_PKG_NAME", "forged")]);
        // Later entries win when the env is applied in order, so the LAST
        // occurrence is the effective value.
        let last = env
            .iter()
            .rev()
            .find(|(k, _)| k == "CARGO_PKG_NAME")
            .map(|(_, v)| v.as_str());
        assert_eq!(last, Some("pkg-a"));
    }

    // Config [env]: plain entries fill holes only, force overrides. The
    // sweep env here stands in for any already-present value.
    #[test]
    fn config_env_respects_force_semantics() {
        let mut rt = runtime_with(BuildRuntimeIndex::default(), HashMap::new());
        rt.config_env = vec![
            ConfigEnvEntry {
                key: "FIXTURE_ROOT".into(),
                value: "/cfg/fixtures".into(),
                force: false,
            },
            ConfigEnvEntry {
                key: "FORCED".into(),
                value: "cfg".into(),
                force: true,
            },
        ];
        let (_, env) = rt.envelope(
            &test_binary(),
            &[("FIXTURE_ROOT", "/sweep/fixtures"), ("FORCED", "sweep")],
        );
        let last = |k: &str| {
            env.iter()
                .rev()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        // Plain entry: the existing value stands, no second entry appended.
        assert_eq!(last("FIXTURE_ROOT"), Some("/sweep/fixtures"));
        // Forced entry: the config value lands after and wins.
        assert_eq!(last("FORCED"), Some("cfg"));
    }
}
