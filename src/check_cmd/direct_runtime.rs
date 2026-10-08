// The launch envelope for executing prebuilt test binaries directly.
//
// The parallel and process-isolated lanes do not re-enter cargo (see
// parallel.rs's module header for why cargo re-entry is unsound): after
// the prebuild, each test binary is executed directly. Cargo does more
// than exec a test binary, though - it launches it with a contract of cwd and
// environment that real tests depend on - and this module is that contract,
// reconstructed from the same sources cargo derives it from:
//
// - cwd = the owning package's root (a fixture test opening
//   `tests/fixtures/x.json` passes under cargo, fails from the workspace root)
// - the dynamic-loader path, in cargo's own order (`Compilation::fill_env`):
//   the build-script link-search dirs that lie inside the output dir, sorted;
//   the output dir (`target/<profile>`); the deps dirs; the toolchain libdir;
//   then the inherited value. Ordering decides which same-named .so loads, and
//   a link-search dir outside the output tree is left out, as cargo leaves it
//   out, because it is likely to shadow a system library (cargo issue 3366)
// - `[env]` from the cargo config chain, with cargo's `force` and `relative`
//   semantics
// - `CARGO_PKG_*` / `CARGO_MANIFEST_DIR` / `CARGO_MANIFEST_PATH` from cargo
//   metadata (unset manifest fields are EMPTY strings, not absent vars -
//   cargo's documented behaviour)
// - `cargo::rustc-env` values from the prebuild's `build-script-executed`
//   messages, keyed by full package id. Not `OUT_DIR`: cargo exports it to
//   build scripts and rustdoc only, never to a test process it launches, so a
//   runtime `std::env::var("OUT_DIR")` is an error under cargo and must be one
//   here (the compile-time `env!("OUT_DIR")` is baked into the binary anyway)
// - runtime `CARGO_BIN_EXE_<name>` for the package's own bins (cargo 1.94+
//   exposes these to the test process at runtime, not only via `env!`)
// - `CARGO` = the cargo executable itself, resolved through rustup when rustup
//   is the `cargo` on PATH: cargo exports its own resolved path, not the proxy
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
//   so the direct-execution lanes REFUSE at resolution time instead - see
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
pub(crate) struct DirectRuntime {
    /// Full package id -> manifest facts, for `CARGO_PKG_*`.
    packages: HashMap<String, PkgRuntimeMeta>,
    index: BuildRuntimeIndex,
    /// Union of every build script's link-search dirs, sorted. Filtered per
    /// binary against its output dir, the way cargo filters them.
    linked_paths: Vec<String>,
    libdir: String,
    /// What `CARGO` is set to: the cargo executable this brokkr runs.
    cargo: String,
    /// `[env]` from the config chain, highest-precedence file first.
    config_env: Vec<ConfigEnvEntry>,
}

impl DirectRuntime {
    /// Assemble the runtime for one sweep: cargo metadata for the package
    /// facts, the prebuild's artifact index, and the cargo config chain.
    pub(crate) fn load(
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
            cargo: cargo_program(project_root, env_refs),
            config_env: cargo_config_env(project_root, env_refs),
        })
    }

    /// The cwd and environment for one test binary, layered onto `env_refs`
    /// (the sweep/project env). Returned env is complete: pass it as the
    /// child's explicit env additions over brokkr's inherited environment.
    ///
    /// Fails when the package's build script ran more than once in this build
    /// with different `rustc-env` output: the stream does not say which run the
    /// executable linked against, and guessing would hand the test another
    /// build's environment.
    pub(crate) fn envelope(
        &self,
        binary: &TestBinary,
        env_refs: &[(&str, &str)],
    ) -> Result<(PathBuf, Vec<(String, String)>), DevError> {
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
        if let Some(runs) = self.index.build_scripts.get(&binary.package_id) {
            let first = runs.first().map(|r| &r.env);
            if runs.iter().any(|r| Some(&r.env) != first) {
                return Err(DevError::Config(format!(
                    "package `{}` ran its build script {} times in this build with different \
                     `rustc-env` output, and cargo's artifact stream does not say which run `{}` \
                     links against - brokkr will not run it directly with a guessed environment",
                    binary.package,
                    runs.len(),
                    binary.label()
                )));
            }
            for (k, v) in first.into_iter().flatten() {
                env.push((k.clone(), v.clone()));
            }
        }
        if let Some(bins) = self.index.bin_exes.get(&binary.package_id) {
            for (name, exe) in bins {
                env.push((format!("CARGO_BIN_EXE_{name}"), exe.clone()));
            }
        }

        let existing = env
            .iter()
            .rev()
            .find(|(k, _)| k == "LD_LIBRARY_PATH")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var("LD_LIBRARY_PATH").ok());
        let search = cargo_search_path(binary, &self.linked_paths, &self.libdir);
        env.push(("LD_LIBRARY_PATH".into(), join_dylib_path(search, existing.as_deref())));

        Ok((cwd, env))
    }
}

/// The output dir a test executable was built into (`target/<profile>`):
/// cargo's `root_output`, which anchors both the loader path and the filter on
/// build-script link-search dirs. Read from where the executable sits - under
/// `<root>/deps/`, or under `<root>/build/<pkg>/<hash>/...` in the newer
/// build-dir layout - rather than assumed to be its grandparent.
fn root_output(executable: &Path) -> PathBuf {
    for dir in executable.ancestors().skip(1) {
        if let (Some(name), Some(parent)) = (dir.file_name(), dir.parent())
            && (name == "deps" || name == "build")
        {
            return parent.to_path_buf();
        }
    }
    executable.parent().map_or_else(PathBuf::new, Path::to_path_buf)
}

/// The loader path cargo builds for a test executable, before the inherited
/// value: link-search dirs inside the output dir (sorted), the output dir, the
/// deps dirs, the toolchain libdir.
fn cargo_search_path(binary: &TestBinary, linked_paths: &[String], libdir: &str) -> Vec<String> {
    let exe = Path::new(&binary.executable);
    let root = root_output(exe);
    let mut search: Vec<String> = linked_paths
        .iter()
        .filter(|p| Path::new(p.as_str()).starts_with(&root))
        .cloned()
        .collect();
    search.push(root.display().to_string());
    let mut deps: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    deps.insert(root.join("deps").display().to_string());
    if let Some(dir) = exe.parent() {
        deps.insert(dir.display().to_string());
    }
    search.extend(deps.into_iter().filter(|d| !search.contains(d)).collect::<Vec<_>>());
    search.push(libdir.to_owned());
    search
}

/// Append the inherited loader path, as cargo does - except that an inherited
/// value already starting with cargo's own entries (brokkr run under cargo, or
/// a nested run) is used as is, rather than doubled.
fn join_dylib_path(search: Vec<String>, existing: Option<&str>) -> String {
    let existing: Vec<String> = existing
        .filter(|e| !e.is_empty())
        .map(|e| e.split(':').map(str::to_owned).collect())
        .unwrap_or_default();
    if !existing.is_empty() && existing.starts_with(&search) {
        return existing.join(":");
    }
    let mut out = search;
    out.extend(existing);
    out.join(":")
}

/// The cargo executable this brokkr run uses, for the child's `CARGO` var.
///
/// brokkr spawns the literal `cargo` from PATH - the sweep's PATH when its
/// `env` sets one. Cargo exports its own resolved executable, so that cargo is
/// what this names: the PATH hit itself, unless the hit is the rustup proxy
/// (the same file as the `rustup` beside it), in which case it is resolved the
/// way the proxy resolves it - from the project root, under the sweep's env, so
/// a toolchain file or a `RUSTUP_TOOLCHAIN` the build saw is the one named. An
/// inherited `CARGO` is not trusted: it names whatever cargo launched brokkr,
/// which is not the one brokkr runs.
fn cargo_program(project_root: &Path, env: &[(&str, &str)]) -> String {
    let path_var = env
        .iter()
        .rev()
        .find(|(k, _)| *k == "PATH")
        .map(|(_, v)| std::ffi::OsString::from(v))
        .or_else(|| std::env::var_os("PATH"));
    // The search `Command` does: the first EXECUTABLE `cargo`, a relative
    // entry taken against where cargo is launched (`project_root`), and the
    // result made absolute - a harness runs from its package directory, where
    // a relative `CARGO` would name another file or none.
    let Some(hit) = path_var.as_deref().and_then(|paths| {
        std::env::split_paths(paths)
            .map(|dir| project_root.join(dir).join("cargo"))
            .find(|c| is_executable_file(c))
    }) else {
        return "cargo".into();
    };
    // The rustup proven to be the proxy is the one asked - not whichever
    // `rustup` PATH happens to reach first.
    let rustup = hit.with_file_name("rustup");
    if is_rustup_proxy(&hit)
        && let Some(rustup) = rustup.to_str()
        && let Ok(out) = output::run_captured_with_env(rustup, &["which", "cargo"], project_root, env)
        && out.status.success()
    {
        let resolved = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !resolved.is_empty() {
            return resolved;
        }
    }
    hit.display().to_string()
}

/// A regular file this process may execute, which is what process launch
/// requires of a PATH candidate before it moves on to the next. `access(2)`
/// rather than the mode bits: an execute bit for someone else (`0641` on a
/// file this user owns) is not one for this user.
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a valid NUL-terminated path for the call's duration.
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// Whether `cargo` is rustup's proxy: the same file (device and inode, through
/// any symlink) as the `rustup` in its directory. rustup installs its proxies
/// as hard links to itself.
fn is_rustup_proxy(cargo: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(rustup) = cargo.parent().map(|d| d.join("rustup")) else {
        return false;
    };
    match (std::fs::metadata(cargo), std::fs::metadata(rustup)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
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
fn cargo_config_env(project_root: &Path, env: &[(&str, &str)]) -> Vec<ConfigEnvEntry> {
    let mut out = Vec::new();
    for path in rustflags::config_paths_under(project_root, env) {
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
///
/// A configured build target (`build.target` or `CARGO_BUILD_TARGET`) refuses
/// too. The runner check above is keyed on the host triple and the envelope's
/// loader path on the host toolchain, and both are wrong for an artifact
/// cargo built for another target - a cross build is exactly where a runner
/// would be configured, under a triple this check would never look at. Any
/// configured target refuses, the host's included: the setting changes where
/// cargo puts the artifacts, which the envelope does not model.
///
/// The environment checked is the one cargo builds under: `env` (the sweep's,
/// which reaches cargo as its process environment) over brokkr's own. A sweep
/// `env` setting a runner or a target is as configured as a shell export.
pub(crate) fn refuse_configured_runner(
    project_root: &Path,
    env: &[(&str, &str)],
) -> Result<(), DevError> {
    let effective = |key: &str| -> Option<String> {
        env.iter()
            .rev()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_owned())
            .or_else(|| std::env::var(key).ok())
            .filter(|v| !v.is_empty())
    };
    if let Some(target) = effective("CARGO_BUILD_TARGET") {
        return Err(DevError::Config(format!(
            "CARGO_BUILD_TARGET={target} is set: cargo builds the tests for that target, and \
             brokkr executes prebuilt test binaries only for the host. Unset it for this run."
        )));
    }
    for path in rustflags::config_paths_under(project_root, env) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = text.parse::<toml::Table>() else {
            continue;
        };
        if doc.get("build").and_then(|b| b.get("target")).is_some() {
            return Err(DevError::Config(format!(
                "{} sets `build.target`: cargo builds the tests for that target, and brokkr \
                 executes prebuilt test binaries only for the host.",
                path.display()
            )));
        }
    }
    let triple = rustflags::host_triple();
    if let Some(t) = &triple {
        let var = format!(
            "CARGO_TARGET_{}_RUNNER",
            t.to_uppercase().replace('-', "_")
        );
        if effective(&var).is_some() {
            return Err(DevError::Config(format!(
                "{var} is set: cargo would run test binaries through that runner, and direct \
                 execution would bypass it. Unset it, or drop `parallel` or \
                 `isolation = \"process\"` from the [[check]] entry."
            )));
        }
    }
    for path in rustflags::config_paths_under(project_root, env) {
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
                     through it, and direct execution would bypass it. Remove the runner for \
                     this host, or drop `parallel` or `isolation = \"process\"` from the \
                     [[check]] entry.",
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
            vec![BuildScriptOut {
                out_dir: Some("/t/debug/build/a/out".into()),
                env: vec![("GENERATED_ENDPOINT".into(), "svc".into())],
                // One inside the output tree, one outside it: cargo keeps only
                // the first on the loader path.
                linked_paths: vec!["/t/debug/build/a/out".into(), "/usr/lib/elsewhere".into()],
            }],
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

        let (cwd, env) =
            rt.envelope(&test_binary(), &[("BROKKR_TEST_BIN_DIR", "/t/debug")]).unwrap();
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
        // Cargo exports OUT_DIR to build scripts and rustdoc, never to a test.
        assert_eq!(get("OUT_DIR"), None);
        assert_eq!(get("GENERATED_ENDPOINT"), Some("svc"));
        assert_eq!(get("CARGO_BIN_EXE_serve-bin"), Some("/t/debug/serve-bin"));
        let ld = get("LD_LIBRARY_PATH").unwrap();
        let parts: Vec<&str> = ld.split(':').collect();
        // Cargo's order: in-tree link-search dirs, the output dir, the deps
        // dir, the toolchain libdir. The out-of-tree link dir is dropped.
        assert_eq!(
            &parts[..4],
            &["/t/debug/build/a/out", "/t/debug", "/t/debug/deps", "/toolchain/lib"]
        );
        assert!(!parts.contains(&"/usr/lib/elsewhere"), "{ld}");
    }

    // The newer build-dir layout puts test executables under
    // `<root>/build/<pkg>/<hash>/`, where "the grandparent" is not the output
    // dir.
    #[test]
    fn the_output_dir_is_found_in_both_executable_layouts() {
        assert_eq!(root_output(Path::new("/t/debug/deps/cli-1")), PathBuf::from("/t/debug"));
        assert_eq!(
            root_output(Path::new("/t/debug/build/pkg/abc123/out/cli-1")),
            PathBuf::from("/t/debug")
        );
    }

    // An inherited loader path that already begins with cargo's entries is
    // a nested run, and is not doubled.
    #[test]
    fn an_inherited_loader_path_with_cargos_prefix_is_not_doubled() {
        let search = vec!["/t/debug".to_owned(), "/lib".to_owned()];
        assert_eq!(join_dylib_path(search.clone(), Some("/t/debug:/lib:/usr/x")), "/t/debug:/lib:/usr/x");
        assert_eq!(join_dylib_path(search.clone(), Some("/usr/x")), "/t/debug:/lib:/usr/x");
        assert_eq!(join_dylib_path(search, None), "/t/debug:/lib");
    }

    // Two runs of one package's build script that disagree leave the
    // executable's environment unknowable, so the envelope refuses rather than
    // letting whichever run was parsed last win.
    #[test]
    fn disagreeing_build_script_runs_refuse_the_envelope() {
        let mut index = BuildRuntimeIndex::default();
        let run = |v: &str| BuildScriptOut {
            out_dir: Some(format!("/t/debug/build/a-{v}/out")),
            env: vec![("MODE".into(), v.into())],
            linked_paths: Vec::new(),
        };
        index
            .build_scripts
            .insert("path+file:///x/a#pkg-a@0.1.0".into(), vec![run("host"), run("target")]);
        let rt = runtime_with(index, HashMap::new());
        let err = rt.envelope(&test_binary(), &[]).unwrap_err().to_string();
        assert!(err.contains("ran its build script 2 times"), "{err}");
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
        let (_, env) = rt.envelope(&test_binary(), &[("CARGO_PKG_NAME", "forged")]).unwrap();
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
        let (_, env) = rt
            .envelope(&test_binary(), &[("FIXTURE_ROOT", "/sweep/fixtures"), ("FORCED", "sweep")])
            .unwrap();
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
