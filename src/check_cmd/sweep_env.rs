/// Project + profile-aware env vars set on every cargo test/build
/// subprocess. Owned `String`s (B7: avoids borrow-from-local traps the
/// previous `Vec<(&str, &str)>` shape allowed) and absolute paths
/// (B8: `CARGO_TARGET_TMPDIR` was relative `target/tmp`, fragile if
/// the cargo subprocess ever ran with a different cwd).
///
/// `target_dir` is cargo's resolved `target_directory` (from
/// `cargo metadata --no-deps`) - workspaces can place it outside the
/// project root, so the caller passes it in rather than us assuming
/// `<project_root>/target`. `profile_dir` is the profile name as it
/// appears under that target: `"debug"` for `brokkr check` (which
/// always tests in the dev profile) and `brokkr test --debug`,
/// `"release"` for the default `brokkr test` invocation.
pub(crate) fn build_test_env(
    project: Option<Project>,
    target_dir: &Path,
    profile_dir: &str,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if matches!(project, Some(Project::Nidhogg)) {
        let tmp = target_dir.join("tmp");
        out.push((
            "CARGO_TARGET_TMPDIR".into(),
            tmp.to_string_lossy().into_owned(),
        ));
    }
    let bin_dir = target_dir.join(profile_dir);
    out.push((
        "BROKKR_TEST_BIN_DIR".into(),
        bin_dir.to_string_lossy().into_owned(),
    ));
    out
}

/// The isolated target dir the sweep's target location implies, or `None` for
/// cargo's shared dir. The location owns both the isolation decision and the
/// stable directory name, so callers cannot disagree about why or where a
/// sweep isolates.
///
/// `meta_target_dir` is cargo's resolved `target_directory` (from `cargo
/// metadata`), not `<project_root>/target`: a workspace `.cargo/config.toml`
/// `[build] target-dir` can place it on another drive entirely, and the
/// isolated dir must sit beside the real one so `brokkr clean` and the plain
/// sweeps find it (S3-20).
/// A package-mode sweep isolates too, even with no `rustflags`: it resolves a
/// different feature graph for the same source, so sharing the common dir would
/// have it rebuild against every ordinary sweep on every run, in both
/// directions. Its dir is shared across the sweep's per-package resolutions -
/// the resolution boundary is not the artifact boundary, and a lane exports one
/// `BROKKR_TEST_BIN_DIR`, so a directory per package would leave a multi-package
/// lane with no single directory holding the binaries its tests spawn.
///
pub(crate) fn isolated_target_dir(
    sweep: &PlannedSweep,
    meta_target_dir: &Path,
) -> Option<std::path::PathBuf> {
    let location = sweep.target_location();
    location.is_isolated().then(|| location.target_dir(meta_target_dir))
}

/// Compose a sweep's `rustflags` with any inherited flags into the env pair to
/// export. Appends to an inherited `CARGO_ENCODED_RUSTFLAGS` (0x1f-separated)
/// when present - cargo ignores `RUSTFLAGS` once the encoded form is set - else
/// to `RUSTFLAGS` (space-joined, matching `make cargo-test-sim`). `None` for an
/// empty flag list.
/// `extra` carries the `[lints] allow` flags when - and only when - the
/// environment is the layer cargo will actually read (see [`crate::rustflags`]).
/// They go last, because rustc resolves conflicting lint levels last-wins and
/// the whole point is to beat the project's `-Dwarnings`. When the config layer
/// is the live one, `extra` is empty here and the flags ride cargo's
/// `--config` instead.
fn composed_rustflags_env(rustflags: &[String], extra: &[String]) -> Option<(String, String)> {
    if rustflags.is_empty() && extra.is_empty() {
        return None;
    }
    if let Ok(existing) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        let mut parts: Vec<String> = if existing.is_empty() {
            Vec::new()
        } else {
            existing.split('\u{1f}').map(str::to_owned).collect()
        };
        parts.extend(rustflags.iter().cloned());
        parts.extend(extra.iter().cloned());
        return Some(("CARGO_ENCODED_RUSTFLAGS".into(), parts.join("\u{1f}")));
    }
    let mut flags = std::env::var("RUSTFLAGS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_default();
    for flag in rustflags.iter().chain(extra) {
        if !flags.is_empty() {
            flags.push(' ');
        }
        flags.push_str(flag);
    }
    Some(("RUSTFLAGS".into(), flags))
}


/// The `CARGO_TARGET_DIR` + `RUSTFLAGS` pair a sweep's `rustflags` imply, or an
/// empty vec when it sets none. Used by the clippy phase (which needs no
/// `BROKKR_TEST_BIN_DIR`); the test phase gets the same knobs plus the bin dir
/// through [`sweep_runtime_env`].
/// `allow_flags` carries the environment's share of the `[lints] allow` flags,
/// on the same terms as [`sweep_runtime_env`] - empty when they ride cargo's
/// `--config`, and empty too for a caller that passes `-A` on its own argv
/// (clippy does). A caller that COMPILES and does neither gets a build without
/// the suppressions, which is a hard error wherever the project's `-Dwarnings`
/// turns one of those lints into one.
pub(crate) fn sweep_cargo_env(
    sweep: &PlannedSweep,
    meta_target_dir: &Path,
    allow_flags: &[String],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(dir) = isolated_target_dir(sweep, meta_target_dir) {
        out.push(("CARGO_TARGET_DIR".into(), dir.to_string_lossy().into_owned()));
    }
    out.extend(composed_rustflags_env(&sweep.rustflags, allow_flags));
    out
}

/// The full base env for one sweep's test/pre-build cargo runs: `build_test_env`
/// computed against the sweep's *effective* target dir (its isolated
/// `target/rustflags-<hash>` when it carries `rustflags`, else cargo's own
/// `meta_target_dir`), plus the `CARGO_TARGET_DIR` / `RUSTFLAGS` overlay. The
/// sweep's own `env` still overlays this via `merged_env`.
/// `allow_flags` is whatever share of the `[lints] allow` flags belongs in the
/// environment ([`crate::rustflags::plumbing`]); empty when they ride cargo's
/// `--config` instead. The caller decides, because the same sink answer also
/// selects the cargo args it must pass alongside these.
pub(crate) fn sweep_runtime_env(
    sweep: &PlannedSweep,
    project: Option<Project>,
    meta_target_dir: &Path,
    profile_dir: &str,
    allow_flags: &[String],
) -> Vec<(String, String)> {
    let isolated = isolated_target_dir(sweep, meta_target_dir);
    let effective: &Path = isolated.as_deref().unwrap_or(meta_target_dir);

    let mut out = build_test_env(project, effective, profile_dir);
    if let Some(dir) = &isolated {
        out.push(("CARGO_TARGET_DIR".into(), dir.to_string_lossy().into_owned()));
    }
    out.extend(composed_rustflags_env(&sweep.rustflags, allow_flags));
    out
}

/// Combine the sweep's profile-defined env with the project's
/// always-set vars (e.g. nidhogg's `CARGO_TARGET_TMPDIR`). Sweep
/// values come first; project values append (so a sweep can shadow a
/// project default if it really needs to).
///
/// The one composition rule for every phase - test, pre-build, clippy,
/// rustdoc and the coverage enumeration - so no two of them can disagree
/// about which side of a collision cargo sees.
pub(crate) fn merged_env(
    sweep_env: &std::collections::BTreeMap<String, String>,
    project_env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> =
        sweep_env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in project_env {
        if !out.iter().any(|(ek, _)| ek == k) {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

