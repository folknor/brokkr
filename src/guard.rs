//! `brokkr guard`: enroll or retire the rustc-wrapper fence.
//!
//! The fence itself is `brokkr-rustc-guard` (`src/bin/rustc_guard.rs`), a
//! second bin target installed next to `brokkr` by `cargo install`. This
//! command manages the one line that activates it user-wide:
//! `build.rustc-wrapper` in `$CARGO_HOME/config.toml`. With it set, every
//! cargo this user runs - PATH-resolved or absolute, rust-analyzer's or an
//! agent's - routes each rustc invocation through the guard, which refuses
//! while the brokkr lock is held unless the cargo runs under brokkr.
//!
//! The wrapper path written is the guard binary sitting next to the running
//! `brokkr` executable - resolved, not guessed, so an install from a dev
//! `target/` tree points there and says so. Changing the configured wrapper
//! invalidates cargo's compile fingerprint, so enrollment costs one full
//! rebuild per target dir, once; the guard stays configured for brokkr's own
//! builds too (the pass/refuse decision is runtime, in the guard), precisely
//! so the fingerprint never oscillates.
//!
//! Edits are comment-preserving (`toml_edit`) and refuse to fight: a
//! different wrapper already configured (sccache, say) is reported, never
//! overwritten - chaining wrappers is a decision for a human.

use std::path::PathBuf;

use crate::error::DevError;
use crate::output;

const WRAPPER_KEY: &str = "rustc-wrapper";
const GUARD_BIN: &str = "brokkr-rustc-guard";

/// The guard protocol this brokkr expects from the enrolled wrapper.
///
/// Printed by the guard as `--brokkr-guard-info` (`src/bin/rustc_guard.rs` -
/// the two sides must be edited together, like `auth_hash`) and compared by
/// [`warn_if_guard_stale`] at locked-command startup and by `brokkr guard`
/// status. Bump it whenever the guard's admission behaviour changes in a way
/// brokkr depends on - the number names required behaviour, not the wire
/// format, because a guard that parses everything correctly but refuses what
/// the current brokkr expects admitted is exactly the stale half this exists
/// to catch.
const GUARD_PROTOCOL: u32 = 1;

/// `brokkr guard [--install|--remove]`: bare shows status.
pub fn cmd_guard(install: bool, remove: bool) -> Result<(), DevError> {
    if install && remove {
        return Err(DevError::Config("--install and --remove are mutually exclusive".into()));
    }
    let config_path = cargo_config_path()?;
    if install {
        return do_install(&config_path);
    }
    if remove {
        return do_remove(&config_path);
    }
    status(&config_path)
}

/// The guard binary next to the running brokkr executable. Resolved, not
/// guessed: `cargo install` puts both bins in one directory, and a dev-tree
/// brokkr finds its dev-tree guard the same way.
fn guard_bin_path() -> Result<PathBuf, DevError> {
    let exe = std::env::current_exe()
        .map_err(|e| DevError::Config(format!("cannot resolve the running brokkr executable: {e}")))?;
    let Some(dir) = exe.parent() else {
        return Err(DevError::Config(format!("{} has no parent directory", exe.display())));
    };
    Ok(dir.join(GUARD_BIN))
}

/// `$CARGO_HOME/config.toml`, honouring the extensionless legacy `config`.
///
/// When BOTH files exist cargo reads the extensionless one (with a warning),
/// so brokkr must edit and inspect that one too - editing `config.toml`
/// while cargo obeys `config` would report a guard that is not actually
/// enrolled. The legacy file therefore wins whenever it exists; `config.toml`
/// is the path only when it is the only candidate (including the
/// neither-exists case, where a fresh install should create the modern name).
fn cargo_config_path() -> Result<PathBuf, DevError> {
    let cargo_home = match std::env::var_os("CARGO_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| DevError::Config("$HOME is not set - cannot locate the cargo config".into()))?;
            PathBuf::from(home).join(".cargo")
        }
    };
    let modern = cargo_home.join("config.toml");
    let legacy = cargo_home.join("config");
    if legacy.exists() {
        return Ok(legacy);
    }
    Ok(modern)
}

fn load_doc(path: &std::path::Path) -> Result<toml_edit::DocumentMut, DevError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(DevError::Config(format!("cannot read {}: {e}", path.display()))),
    };
    text.parse()
        .map_err(|e| DevError::Config(format!("cannot parse {}: {e}", path.display())))
}

/// The configured `build.rustc-wrapper`, if any.
fn configured_wrapper(doc: &toml_edit::DocumentMut) -> Option<String> {
    doc.get("build")?.get(WRAPPER_KEY)?.as_str().map(str::to_owned)
}

fn do_install(config_path: &std::path::Path) -> Result<(), DevError> {
    let guard = guard_bin_path()?;
    if !guard.exists() {
        return Err(DevError::Config(format!(
            "{} not found next to the running brokkr - run `brokkr install` in the brokkr repo first",
            guard.display()
        )));
    }
    let mut doc = load_doc(config_path)?;
    if let Some(existing) = configured_wrapper(&doc) {
        if existing == guard.display().to_string() {
            output::lock_msg("guard already installed");
            status(config_path)?;
            return Ok(());
        }
        return Err(DevError::Config(format!(
            "{} already sets build.{WRAPPER_KEY} = \"{existing}\" - refusing to overwrite a wrapper brokkr did not write",
            config_path.display()
        )));
    }
    if doc.get("build").is_none() {
        doc["build"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["build"][WRAPPER_KEY] = toml_edit::value(guard.display().to_string());
    if let Some(dir) = config_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(config_path, doc.to_string())?;
    output::lock_msg(&format!("build.{WRAPPER_KEY} = \"{}\" written to {}", guard.display(), config_path.display()));
    output::lock_msg("every cargo this user runs now routes rustc through the guard; the wrapper change invalidates cargo's fingerprint, so the next build in each target dir is a full rebuild (once)");
    Ok(())
}

fn do_remove(config_path: &std::path::Path) -> Result<(), DevError> {
    let mut doc = load_doc(config_path)?;
    let Some(existing) = configured_wrapper(&doc) else {
        output::lock_msg(&format!("no build.{WRAPPER_KEY} in {} - nothing to remove", config_path.display()));
        return Ok(());
    };
    if !existing.ends_with(GUARD_BIN) {
        return Err(DevError::Config(format!(
            "build.{WRAPPER_KEY} = \"{existing}\" is not the brokkr guard - refusing to remove a wrapper brokkr did not write"
        )));
    }
    if let Some(build) = doc.get_mut("build").and_then(toml_edit::Item::as_table_mut) {
        build.remove(WRAPPER_KEY);
        if build.is_empty() {
            doc.as_table_mut().remove("build");
        }
    }
    std::fs::write(config_path, doc.to_string())?;
    output::lock_msg(&format!("build.{WRAPPER_KEY} removed from {}", config_path.display()));
    Ok(())
}

/// How the enrolled guard answered the `--brokkr-guard-info` handshake.
enum GuardProbe {
    /// Answered with the protocol this brokkr expects.
    Current,
    /// Answered, but with a different protocol number.
    Mismatch(u32),
    /// Did not answer the handshake at all - an old guard treats the flag as
    /// the executable to wrap and dies at exec, a wedged one times out. Both
    /// mean the enrolled binary is not the one this brokkr shipped with.
    Stale(String),
}

/// Exec the enrolled guard's handshake, bounded so an old guard that
/// misparses the flag - or one blocked on a draining compile lease - becomes
/// a distinct warning rather than a hang.
fn probe_guard(wrapper: &str) -> GuardProbe {
    let capture = output::run_captured_with_env_and_deadline(
        wrapper,
        &["--brokkr-guard-info"],
        std::path::Path::new("/"),
        &[],
        std::time::Duration::from_secs(3),
        None,
        false,
    );
    let capture = match capture {
        Ok(c) => c,
        Err(e) => return GuardProbe::Stale(format!("could not be executed: {e}")),
    };
    if capture.killed_on_deadline {
        return GuardProbe::Stale("did not answer the handshake within 3s".into());
    }
    let stdout = String::from_utf8_lossy(&capture.captured.stdout);
    let Some(first) = stdout.lines().next() else {
        return GuardProbe::Stale("answered the handshake with no output".into());
    };
    let Some(proto) = first
        .strip_prefix("brokkr-rustc-guard protocol=")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<u32>().ok())
    else {
        return GuardProbe::Stale("does not speak the handshake (predates it, or is not the guard)".into());
    };
    if proto == GUARD_PROTOCOL {
        GuardProbe::Current
    } else {
        GuardProbe::Mismatch(proto)
    }
}

/// The stale-guard warning for the enrolled wrapper, or `None` when the
/// enrolled guard is current or no brokkr guard is enrolled at all (an
/// unenrolled or foreign wrapper is `brokkr guard`'s business, not a
/// per-command warning).
fn stale_warning() -> Option<String> {
    let config_path = cargo_config_path().ok()?;
    let doc = load_doc(&config_path).ok()?;
    let wrapper = configured_wrapper(&doc)?;
    if !wrapper.ends_with(GUARD_BIN) {
        return None;
    }
    match probe_guard(&wrapper) {
        GuardProbe::Current => None,
        GuardProbe::Mismatch(proto) => Some(format!(
            "the enrolled rustc guard speaks protocol {proto}, this brokkr expects {GUARD_PROTOCOL} - \
             run `brokkr install` in the brokkr repo to update {wrapper}"
        )),
        GuardProbe::Stale(why) => Some(format!(
            "the enrolled rustc guard {wrapper} {why} - it predates this brokkr; \
             run `brokkr install` in the brokkr repo to update it"
        )),
    }
}

/// Warn (once per process) if the enrolled guard is stale. Called after every
/// locked-command acquisition: the guard and brokkr install together, but
/// nothing forces them to, and a half-upgraded pair fails in ways that read
/// as anything but staleness. Detection at the moment checked, not a
/// guarantee against replacement afterwards; never fails the run.
pub fn warn_if_guard_stale() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Some(warning) = stale_warning() {
            output::lock_msg(&format!("WARNING: {warning}"));
        }
    });
}

fn status(config_path: &std::path::Path) -> Result<(), DevError> {
    let doc = load_doc(config_path)?;
    match configured_wrapper(&doc) {
        Some(w) if w.ends_with(GUARD_BIN) => {
            let exists = PathBuf::from(&w).exists();
            output::lock_msg(&format!("guard installed: build.{WRAPPER_KEY} = \"{w}\" in {}", config_path.display()));
            if !exists {
                output::lock_msg("WARNING: the configured guard binary does not exist - cargo will fail until `brokkr install` restores it or `brokkr guard --remove` unsets it");
            } else {
                match probe_guard(&w) {
                    GuardProbe::Current => {
                        output::lock_msg(&format!("guard protocol {GUARD_PROTOCOL}: current"));
                    }
                    GuardProbe::Mismatch(proto) => output::lock_msg(&format!(
                        "WARNING: guard speaks protocol {proto}, this brokkr expects {GUARD_PROTOCOL} - run `brokkr install` in the brokkr repo"
                    )),
                    GuardProbe::Stale(why) => output::lock_msg(&format!(
                        "WARNING: guard {why} - run `brokkr install` in the brokkr repo"
                    )),
                }
            }
        }
        Some(w) => {
            output::lock_msg(&format!("a different wrapper is configured: build.{WRAPPER_KEY} = \"{w}\" - the guard is NOT active"));
        }
        None => {
            output::lock_msg(&format!("guard not installed (no build.{WRAPPER_KEY} in {}) - `brokkr guard --install` enrolls it", config_path.display()));
        }
    }
    Ok(())
}
