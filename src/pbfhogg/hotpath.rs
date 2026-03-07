//! Hotpath profiling: function-level timing and allocation instrumentation.
//!
//! Consolidates `run-hotpath.sh`, `run-hotpath-alloc.sh`, and
//! `run-hotpath-germany.sh` into a single command.

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig, BenchHarness};
use crate::output;

// ---------------------------------------------------------------------------
// Test suite definition
// ---------------------------------------------------------------------------

struct HotpathTest {
    label: &'static str,
    args: Vec<String>,
}

fn build_test_suite(
    binary_str: &str,
    pbf_str: &str,
    osc_str: &str,
    merged_str: &str,
    cat_output_str: &str,
) -> Vec<HotpathTest> {
    vec![
        HotpathTest {
            label: "inspect-tags",
            args: vec![
                binary_str.into(),
                "inspect".into(),
                "tags".into(),
                pbf_str.into(),
            ],
        },
        HotpathTest {
            label: "check-refs",
            args: vec![
                binary_str.into(),
                "check".into(),
                "--refs".into(),
                pbf_str.into(),
            ],
        },
        HotpathTest {
            label: "cat",
            args: vec![
                binary_str.into(),
                "cat".into(),
                pbf_str.into(),
                "--type".into(),
                "node,way,relation".into(),
                "--compression".into(),
                "zlib".into(),
                "-o".into(),
                cat_output_str.into(),
            ],
        },
        HotpathTest {
            label: "apply-changes-zlib",
            args: vec![
                binary_str.into(),
                "apply-changes".into(),
                pbf_str.into(),
                osc_str.into(),
                "--compression".into(),
                "zlib".into(),
                "-o".into(),
                merged_str.into(),
            ],
        },
        HotpathTest {
            label: "apply-changes-none",
            args: vec![
                binary_str.into(),
                "apply-changes".into(),
                pbf_str.into(),
                osc_str.into(),
                "--compression".into(),
                "none".into(),
                "-o".into(),
                merged_str.into(),
            ],
        },
    ]
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the hotpath profiling suite.
///
/// Runs each test `runs` times, recording results through the bench harness.
/// When `alloc` is true, the binary is expected to be built with `hotpath-alloc`
/// feature; the variant name gets a `/alloc` suffix.
///
/// Tests that fail are reported but do not abort the suite — remaining tests
/// continue to run.  The command exits successfully if at least one test passed.
/// Available test labels for `--test` filtering.
pub const TEST_LABELS: &[&str] = &["inspect-tags", "check-refs", "cat", "apply-changes-zlib", "apply-changes-none"];

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    osc_path: &Path,
    file_mb: f64,
    runs: usize,
    alloc: bool,
    scratch_dir: &Path,
    project_root: &Path,
    test_filter: Option<&str>,
) -> Result<(), DevError> {
    if let Some(name) = test_filter {
        if !TEST_LABELS.contains(&name) {
            return Err(DevError::Config(format!(
                "unknown hotpath test '{name}'. Available: {}",
                TEST_LABELS.join(", ")
            )));
        }
    }
    std::fs::create_dir_all(scratch_dir)?;
    let merged_path = scratch_dir.join("hotpath-merged.osm.pbf");
    let cat_output_path = scratch_dir.join("hotpath-cat-output.osm.pbf");

    let binary_str = binary
        .to_str()
        .ok_or_else(|| DevError::Config("binary path is not valid UTF-8".into()))?;
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;
    let osc_str = osc_path
        .to_str()
        .ok_or_else(|| DevError::Config("OSC path is not valid UTF-8".into()))?;
    let merged_str = merged_path
        .to_str()
        .ok_or_else(|| DevError::Config("merged path is not valid UTF-8".into()))?;
    let cat_output_str = cat_output_path
        .to_str()
        .ok_or_else(|| DevError::Config("cat output path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let tests = build_test_suite(binary_str, pbf_str, osc_str, merged_str, cat_output_str);

    let mut passed = 0usize;
    let mut failed: Vec<(&str, String)> = Vec::new();

    for test in &tests {
        if let Some(filter) = test_filter {
            if test.label != filter {
                continue;
            }
        }
        let variant_suffix = crate::harness::hotpath_variant_suffix(alloc);
        let variant = format!("{}{variant_suffix}", test.label);

        // Build args without the binary path (first element) for the subprocess.
        let subprocess_args: Vec<&str> = test.args[1..].iter().map(String::as_str).collect();

        let config = BenchConfig {
            command: "hotpath".into(),
            variant: Some(variant),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: "release".into(),
            runs,
            cli_args: Some(crate::harness::format_cli_args(&binary.display().to_string(), &subprocess_args)),
            metadata: vec![KvPair::text("meta.alloc", alloc.to_string()), KvPair::text("meta.test", test.label)],
        };

        let result = harness.run_internal(&config, |_i| {
            output::hotpath_msg(test.label);
            let program = binary.display().to_string();
            let (result, _stderr) = harness::run_hotpath_capture(&program, &subprocess_args, scratch_dir, project_root, &[])?;
            Ok(result)
        });

        match result {
            Ok(_) => passed += 1,
            Err(e) => {
                output::error(&format!("{}: {e}", test.label));
                failed.push((test.label, e.to_string()));
            }
        }
    }

    // Clean up output files (ignore errors if they don't exist).
    std::fs::remove_file(&merged_path).ok();
    std::fs::remove_file(&cat_output_path).ok();

    if !failed.is_empty() {
        output::hotpath_msg(&format!(
            "{passed}/{} tests passed ({} failed: {})",
            tests.len(),
            failed.len(),
            failed.iter().map(|(l, _)| *l).collect::<Vec<_>>().join(", "),
        ));
    }

    if passed == 0 {
        Err(DevError::Subprocess {
            program: "hotpath".into(),
            code: Some(1),
            stderr: "all hotpath tests failed".into(),
        })
    } else {
        Ok(())
    }
}
