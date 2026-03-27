//! Read-only filesystem verification for nidhogg.
//!
//! Replaces `test-readonly.sh`. Makes the geocode index read-only, starts the
//! server, runs geocode and API query tests, then restores permissions.

use std::path::Path;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the read-only filesystem verification.
///
/// 1. Stop any running server
/// 2. Make geocode_index/ read-only
/// 3. Start server
/// 4. Run geocode + API tests
/// 5. Restore permissions (always, even on failure)
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

    // Make read-only.
    output::verify_msg(&format!("making read-only: {}", geocode_dir.display()));
    set_permissions(&geocode_dir, false, project_root)?;

    // Run tests with cleanup guarantee.
    let test_result = run_tests(binary, data_dir, port, project_root, bbox);

    // Always restore permissions and stop server.
    output::verify_msg("restoring permissions");
    let restore_result = set_permissions(&geocode_dir, true, project_root);
    let stop_result = super::server::stop(project_root);

    // Report results.
    // Prioritize the test error, but also report cleanup errors.
    if let Err(e) = &restore_result {
        output::error(&format!("failed to restore permissions: {e}"));
    }
    if let Err(e) = &stop_result {
        output::error(&format!("failed to stop server: {e}"));
    }

    test_result
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
    super::server::serve(binary, data_dir, None, port, project_root)?;

    let mut passed = 0u32;
    let mut failed = 0u32;

    // Geocode tests.
    let geocode_queries = super::client::GEOCODE_TEST_QUERIES;
    for query in geocode_queries {
        match run_geocode_check(port, query) {
            Ok(true) => {
                output::verify_msg(&format!("PASS  geocode '{query}'"));
                passed += 1;
            }
            Ok(false) => {
                output::verify_msg(&format!("FAIL  geocode '{query}'"));
                failed += 1;
            }
            Err(e) => {
                output::verify_msg(&format!("FAIL  geocode '{query}': {e}"));
                failed += 1;
            }
        }
    }

    // API query test.
    match run_query_check(port, bbox) {
        Ok(true) => {
            output::verify_msg("PASS  API query");
            passed += 1;
        }
        Ok(false) => {
            output::verify_msg("FAIL  API query");
            failed += 1;
        }
        Err(e) => {
            output::verify_msg(&format!("FAIL  API query: {e}"));
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

/// Check a single geocode query returns non-empty results.
fn run_geocode_check(port: u16, query: &str) -> Result<bool, DevError> {
    let url = super::client::geocode_url(port, query);

    let stdout = match super::client::curl_get(&url) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };

    let parsed: Result<serde_json::Value, _> = serde_json::from_str(&stdout);

    match parsed {
        Ok(val) => {
            let non_empty = val.as_array().is_some_and(|arr| !arr.is_empty());
            Ok(non_empty)
        }
        Err(_) => Ok(false),
    }
}

/// Check a single API query returns non-empty elements.
fn run_query_check(port: u16, bbox: &str) -> Result<bool, DevError> {
    let url = super::client::query_url(port);
    let body = super::client::default_api_query(bbox)?;

    let stdout = match super::client::curl_post(&url, &body) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };

    let parsed: Result<serde_json::Value, _> = serde_json::from_str(&stdout);

    match parsed {
        Ok(val) => Ok(super::client::element_count(&val) > 0),
        Err(_) => Ok(false),
    }
}

/// Set permissions on a directory tree: writable (true) or read-only (false).
fn set_permissions(dir: &Path, writable: bool, project_root: &Path) -> Result<(), DevError> {
    let mode_arg = if writable { "u+w" } else { "u-w" };
    let dir_str = dir.display().to_string();

    let result = std::process::Command::new("chmod")
        .args(["-R", mode_arg, &dir_str])
        .current_dir(project_root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| DevError::Subprocess {
            program: "chmod".into(),
            code: None,
            stderr: e.to_string(),
        })?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(DevError::Subprocess {
            program: "chmod".into(),
            code: result.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    Ok(())
}
