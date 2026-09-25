//! Read-only filesystem verification for nidhogg.
//!
//! Replaces `test-readonly.sh`. Makes the geocode index read-only, starts the
//! server, runs geocode and API query tests, then restores permissions.
//!
//! The restore is exact and unconditional: [`ReadOnlyGuard`] records every
//! entry's mode before clearing its owner-write bit and puts each mode back
//! verbatim on drop. The shell version's `chmod -R u+w` restore granted
//! owner-write to files that never had it, and ran only if the script got
//! that far. A [`SigtermGuard`] covers the test window so Ctrl-C and `brokkr
//! kill` become an orderly early exit that unwinds through the guard, rather
//! than the default terminate action that would skip it and leave the index
//! read-only.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::output;
use crate::shutdown::{self, SigtermGuard};

/// The owner-write bit - the one bit the shell script's `chmod -R u-w`
/// cleared, and the one that decides writability for the owning user the
/// server runs as.
const OWNER_WRITE: u32 = 0o200;

/// Permission bits `chmod(2)` honours; everything above is file type.
const MODE_BITS: u32 = 0o7777;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the read-only filesystem verification.
///
/// 1. Stop any running server
/// 2. Make geocode_index/ read-only (recording every original mode)
/// 3. Start server
/// 4. Run geocode + API tests
/// 5. Restore the recorded modes exactly (always, even on failure or signal)
/// 6. Stop server
pub fn run(
    binary: &Path,
    data_dir: &str,
    port: u16,
    project_root: &Path,
    bbox: &str,
) -> Result<(), DevError> {
    output::verify_msg("=== read-only filesystem test ===");

    // Stop any running server.
    super::server::stop(project_root)?;

    // Find geocode index directory.
    let geocode_dir = project_root.join(data_dir).join("geocode_index");
    if !geocode_dir.exists() {
        return Err(DevError::Config(format!(
            "geocode index not found at {}",
            geocode_dir.display(),
        )));
    }

    // Declared before the permission guard so it is dropped after it:
    // signals stay caught while the modes are being restored.
    let signals = SigtermGuard::install();

    // Make read-only.
    output::verify_msg(&format!("making read-only: {}", geocode_dir.display()));
    let mut readonly = ReadOnlyGuard::apply(&geocode_dir)?;

    // Run tests; the guard restores on every exit path from here on.
    let test_result = run_tests(binary, data_dir, port, project_root, bbox);

    // Always restore permissions and stop server.
    output::verify_msg("restoring permissions");
    let restore_result = readonly.restore();
    let stop_result = super::server::stop(project_root);
    let interrupted = shutdown::is_shutdown_requested();
    drop(signals);

    if let Err(e) = &stop_result {
        output::error(&format!("failed to stop server: {e}"));
    }

    // An interruption or a test failure takes priority; a restore failure
    // under it is printed so it is not lost. With the tests green, a failed
    // restore IS the result - an index left read-only is not a pass.
    let primary = if interrupted {
        Err(DevError::Interrupted)
    } else {
        test_result
    };
    match primary {
        Err(e) => {
            if let Err(re) = &restore_result {
                output::error(&format!("failed to restore permissions: {re}"));
            }
            Err(e)
        }
        Ok(()) => restore_result,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run the actual verification tests (geocode + API query).
fn run_tests(
    binary: &Path,
    data_dir: &str,
    port: u16,
    project_root: &Path,
    bbox: &str,
) -> Result<(), DevError> {
    // Start server.
    super::server::serve(binary, Some(data_dir), None, port, project_root)?;

    let mut passed = 0u32;
    let mut failed = 0u32;

    // Geocode tests.
    let geocode_queries = super::client::GEOCODE_TEST_QUERIES;
    for query in geocode_queries {
        if shutdown::is_shutdown_requested() {
            return Err(DevError::Interrupted);
        }
        match run_geocode_check(port, query) {
            Ok(()) => {
                output::verify_msg(&format!("PASS  geocode '{query}'"));
                passed += 1;
            }
            Err(why) => {
                output::verify_msg(&format!("FAIL  geocode '{query}': {why}"));
                failed += 1;
            }
        }
    }

    if shutdown::is_shutdown_requested() {
        return Err(DevError::Interrupted);
    }

    // API query test.
    match run_query_check(port, bbox) {
        Ok(()) => {
            output::verify_msg("PASS  API query");
            passed += 1;
        }
        Err(why) => {
            output::verify_msg(&format!("FAIL  API query: {why}"));
            failed += 1;
        }
    }

    output::verify_msg(&format!("readonly: {passed} passed, {failed} failed"));

    if failed > 0 {
        return Err(DevError::Config(format!(
            "read-only verification failed: {failed} test(s) failed"
        )));
    }

    output::verify_msg("read-only verification passed");
    Ok(())
}

/// Check a single geocode query returns non-empty results. `Err` names why
/// it did not - the curl failure, a non-JSON body, or an empty result.
fn run_geocode_check(port: u16, query: &str) -> Result<(), String> {
    let url = super::client::geocode_url(port, query);
    let stdout =
        super::client::curl_get(&url).map_err(|e| super::client::describe_curl_error(&e))?;
    let val: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| format!("response is not JSON: {e}"))?;
    match val.as_array() {
        Some(arr) if !arr.is_empty() => Ok(()),
        Some(_) => Err("0 results".into()),
        None => Err("response is not an array".into()),
    }
}

/// Check a single API query returns non-empty elements. `Err` names why not.
fn run_query_check(port: u16, bbox: &str) -> Result<(), String> {
    let url = super::client::query_url(port);
    let body = super::client::default_api_query(bbox).map_err(|e| e.to_string())?;
    let stdout = super::client::curl_post(&url, &body)
        .map_err(|e| super::client::describe_curl_error(&e))?;
    let val: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| format!("response is not JSON: {e}"))?;
    match val.get("elements").and_then(serde_json::Value::as_array) {
        Some(arr) if !arr.is_empty() => Ok(()),
        Some(_) => Err("0 elements".into()),
        None => Err("response has no \"elements\" array".into()),
    }
}

/// Clears the owner-write bit across a tree and puts every original mode back
/// exactly - on [`ReadOnlyGuard::restore`] or, failing that, on drop.
///
/// Symlinks are skipped (never followed): `chmod` on a link changes its
/// target, which may live outside the tree.
struct ReadOnlyGuard {
    /// Every entry of the tree with its mode as found, permission bits only.
    entries: Vec<(PathBuf, u32)>,
    restored: bool,
}

impl ReadOnlyGuard {
    /// Record every mode first, then clear owner-write on each. A failure
    /// part-way through returns an error, and the half-built guard's drop
    /// restores whatever had already been changed.
    fn apply(root: &Path) -> Result<Self, DevError> {
        let mut entries = Vec::new();
        collect_modes(root, &mut entries)?;
        let guard = Self {
            entries,
            restored: false,
        };
        for (path, mode) in &guard.entries {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !OWNER_WRITE))?;
        }
        Ok(guard)
    }

    /// Put every recorded mode back. Attempts every entry even after a
    /// failure, then reports all failures together. Idempotent: a second
    /// call (the drop after an explicit restore) does nothing.
    fn restore(&mut self) -> Result<(), DevError> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        let failures: Vec<String> = self
            .entries
            .iter()
            .filter_map(|(path, mode)| {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode))
                    .err()
                    .map(|e| format!("{}: {e}", path.display()))
            })
            .collect();
        if failures.is_empty() {
            return Ok(());
        }
        Err(DevError::Config(format!(
            "could not restore the original mode of {} entr{}:\n  {}",
            failures.len(),
            if failures.len() == 1 { "y" } else { "ies" },
            failures.join("\n  ")
        )))
    }
}

impl Drop for ReadOnlyGuard {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            output::error(&format!("failed to restore permissions: {e}"));
        }
    }
}

/// Depth-first walk recording `(path, mode & MODE_BITS)` for the root and
/// every directory and file beneath it. Symlinks are not recorded or entered.
fn collect_modes(path: &Path, out: &mut Vec<(PathBuf, u32)>) -> Result<(), DevError> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    out.push((path.to_owned(), meta.permissions().mode() & MODE_BITS));
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            collect_modes(&entry?.path(), out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & MODE_BITS
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn restore_puts_back_exact_modes_and_grants_nothing_new() {
        let root = crate::test_scratch::scratch("nidhogg-verify-readonly", "exact_restore");
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let writable = sub.join("writable");
        let readonly = sub.join("readonly");
        std::fs::write(&writable, "w").unwrap();
        std::fs::write(&readonly, "r").unwrap();
        set_mode(&writable, 0o640);
        set_mode(&readonly, 0o440);
        let before: Vec<u32> = [&root, &sub, &writable, &readonly]
            .iter()
            .map(|p| mode_of(p))
            .collect();

        let mut guard = ReadOnlyGuard::apply(&root).unwrap();
        for p in [&root, &sub, &writable, &readonly] {
            assert_eq!(mode_of(p) & OWNER_WRITE, 0, "{} still owner-writable", p.display());
        }
        guard.restore().unwrap();
        drop(guard);

        let after: Vec<u32> = [&root, &sub, &writable, &readonly]
            .iter()
            .map(|p| mode_of(p))
            .collect();
        assert_eq!(before, after);
        // The old `chmod -R u+w` restore would have made this 0o640.
        assert_eq!(mode_of(&readonly), 0o440);
    }

    #[test]
    fn drop_restores_without_an_explicit_restore() {
        let root = crate::test_scratch::scratch("nidhogg-verify-readonly", "drop_restore");
        let file = root.join("f");
        std::fs::write(&file, "x").unwrap();
        set_mode(&file, 0o600);
        let dir_mode = mode_of(&root);
        {
            let _guard = ReadOnlyGuard::apply(&root).unwrap();
            assert_eq!(mode_of(&file), 0o400);
        }
        assert_eq!(mode_of(&file), 0o600);
        assert_eq!(mode_of(&root), dir_mode);
    }
}
