//! Sweep environment composition: the env every cargo run of a sweep sees,
//! the isolated target dir a sweep with `rustflags` gets, and
//! `place_sweep_rustdocflags`, which puts a sweep's cfg rustflags into
//! rustdocflags for the rustdoc phase and every test-lane invocation.

use std::path::Path;

use crate::profile::PlannedSweep;
use crate::project::Project;

/// Project + profile-aware env vars set on every cargo test/build
/// subprocess. Owned `String`s (a `Vec<(&str, &str)>` invites
/// borrow-from-local traps) and absolute paths (a relative
/// `CARGO_TARGET_TMPDIR` breaks if the cargo subprocess runs with a
/// different cwd).
///
/// `target_dir` is cargo's resolved `target_directory` (from
/// `cargo metadata --no-deps`) - workspaces can place it outside the
/// project root, so the caller passes it in rather than us assuming
/// `<project_root>/target`. `profile_dir` is the profile name as it
/// appears under that target: `"debug"` for `brokkr check` (which
/// defaults to the dev profile unless the sweep pins release) and
/// `brokkr test --debug`,
/// `"release"` for the default `brokkr test` invocation.
pub(crate) fn build_test_env(
    project: Option<&Project>,
    target_dir: &Path,
    profile_dir: &str,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if matches!(project, Some(Project::Nidhogg)) {
        let tmp = target_dir.join(crate::target_namespace::TMP);
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
/// sweeps find it.
///
/// A package-mode sweep isolates too, even with no `rustflags`: it resolves a
/// different feature graph for the same source, so sharing the common dir would
/// have it rebuild against every ordinary sweep on every run, in both
/// directions. Its dir is shared across the sweep's per-package resolutions -
/// the resolution boundary is not the artifact boundary, and a lane exports one
/// `BROKKR_TEST_BIN_DIR`, so a directory per package would leave a multi-package
/// lane with no single directory holding the binaries its tests spawn.
pub(crate) fn isolated_target_dir(
    sweep: &PlannedSweep,
    meta_target_dir: &Path,
) -> Option<std::path::PathBuf> {
    let location = sweep.target_location();
    location
        .is_isolated()
        .then(|| location.target_dir(meta_target_dir))
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
/// A live rustflags value that is not UTF-8 refuses composition; the launcher
/// cannot preserve it through the string environment API.
fn composed_rustflags_env(
    rustflags: &[String],
    extra: &[String],
) -> Result<Option<(String, String)>, crate::error::DevError> {
    if rustflags.is_empty() && extra.is_empty() {
        return Ok(None);
    }
    let additions: Vec<String> = rustflags.iter().chain(extra).cloned().collect();
    crate::rustflags::RustflagsEnv::process().compose(&additions)
}

/// The `CARGO_TARGET_DIR` + `RUSTFLAGS` pair a sweep's `rustflags` imply, or an
/// empty vec when it sets none. Composition can return a config error when it
/// cannot safely append to an inherited rustflags source. Used by clippy (which needs no
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
) -> Result<Vec<(String, String)>, crate::error::DevError> {
    resolve_sweep_env(false, sweep, None, meta_target_dir, "debug", allow_flags)
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
    project: Option<&Project>,
    meta_target_dir: &Path,
    profile_dir: &str,
    allow_flags: &[String],
) -> Result<Vec<(String, String)>, crate::error::DevError> {
    resolve_sweep_env(
        true,
        sweep,
        project,
        meta_target_dir,
        profile_dir,
        allow_flags,
    )
}

fn resolve_sweep_env(
    runtime_env: bool,
    sweep: &PlannedSweep,
    project: Option<&Project>,
    meta_target_dir: &Path,
    profile_dir: &str,
    allow_flags: &[String],
) -> Result<Vec<(String, String)>, crate::error::DevError> {
    let isolated = isolated_target_dir(sweep, meta_target_dir);
    let effective = isolated.as_deref().unwrap_or(meta_target_dir);
    // A target sweep's own artifacts land under `<target dir>/<triple>/`;
    // `BROKKR_TEST_BIN_DIR` names that directory. Host units cargo builds
    // alongside (build scripts, proc-macro test harnesses) stay outside it,
    // and a host harness that spawns a binary from it gets the target's.
    let profile_dir = match sweep.target() {
        Some(triple) => format!("{triple}/{profile_dir}"),
        None => profile_dir.to_owned(),
    };
    let mut out = if runtime_env {
        build_test_env(project, effective, &profile_dir)
    } else {
        Vec::new()
    };
    if let Some(dir) = isolated {
        out.push((
            "CARGO_TARGET_DIR".into(),
            dir.to_string_lossy().into_owned(),
        ));
    }
    out.extend(composed_rustflags_env(&sweep.rustflags, allow_flags)?);
    Ok(out)
}

/// Carry a sweep's `--cfg` / `--check-cfg` rustflags, plus `extra` (brokkr's
/// own rustdoc flags), into the rustdocflags of one cargo invocation.
///
/// Cargo hands `RUSTFLAGS` to rustc and never to rustdoc, so without this
/// `cargo doc` documents a cfg sweep with the cfg off and `cargo test`
/// collects and compiles its doctests with the cfg off, against a library
/// built with it on. Only the cfg entries cross: rustdoc resolves `cfg` as
/// the build does, and every other rustc flag stays rustc's.
///
/// `env` is the invocation's final environment (sweep env over brokkr's), and
/// `args` its cargo argv before the selection. `target` supplies the sweep
/// triple's cfg facts, `None` for a host sweep; it is called exactly once when
/// there is something to place and never otherwise, so a run with nothing to
/// carry queries no target facts. The flags are placed by
/// [`crate::rustflags::place`]: appended to the live env source - the pair
/// then replaces the key in `env`, so a sweep's own `RUSTDOCFLAGS` cannot undo
/// it - else as `--config` at the live config layer, pushed onto `args`.
/// Cargo applies rustdocflags by the same per-unit rule as rustflags, so a
/// unit the sweep's rustflags never reach (a proc-macro under `--target`)
/// gets neither. Returns the env pair set, for the caller's command line.
///
/// A layer chosen on a guess (an unjudgeable target table in the cargo
/// config) is warned about once per sweep when the sweep carries cfg flags:
/// should that table apply, the flags are inert and rustdoc runs without the
/// cfg. A known unsupported case, not a satisfied promise; a `RUSTDOCFLAGS`
/// in the sweep's `env` makes the placement exact.
pub(crate) fn place_sweep_rustdocflags<'t>(
    project_root: &Path,
    sweep: &PlannedSweep,
    target: impl FnOnce() -> Result<
        Option<std::borrow::Cow<'t, crate::target_platform::TargetFacts>>,
        crate::error::DevError,
    >,
    extra: &[&str],
    env: &mut Vec<(String, String)>,
    args: &mut Vec<String>,
) -> Result<Option<(String, String)>, crate::error::DevError> {
    let cfg = crate::rustflags::cfg_args(&sweep.rustflags);
    let additions: Vec<String> = cfg
        .iter()
        .cloned()
        .chain(extra.iter().map(|s| (*s).to_owned()))
        .collect();
    if additions.is_empty() {
        return Ok(None);
    }
    let target = target()?;
    let placement = {
        let refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        crate::rustflags::place(
            crate::rustflags::FlagKind::Rustdoc,
            project_root,
            target.as_deref(),
            crate::rustflags::EffectiveEnv::new(&refs),
            &additions,
        )?
    };
    if !placement.certain && !cfg.is_empty() && first_rustdoc_guess_notice(&sweep.label) {
        crate::output::warn(&format!(
            "sweep '{}': a `target.*.rustdocflags` table in the cargo config could not be \
             judged, so its rustflags cfg rides `--config build.rustdocflags` and may not reach \
             rustdoc. If that table applies, the rustdoc phase and doctests run without it. A \
             RUSTDOCFLAGS entry in the sweep's env makes the placement exact.",
            sweep.label
        ));
    }
    args.extend(placement.args);
    if let Some((key, value)) = &placement.env {
        env.retain(|(k, _)| k != key);
        env.push((key.clone(), value.clone()));
    }
    Ok(placement.env)
}

/// True the first time a sweep label asks, for the once-per-run guess warning.
fn first_rustdoc_guess_notice(label: &str) -> bool {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(label.to_owned())
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
    let mut out: Vec<(String, String)> = sweep_env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (k, v) in project_env {
        if !out.iter().any(|(ek, _)| ek == k) {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}
