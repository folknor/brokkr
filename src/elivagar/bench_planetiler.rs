//! Benchmark: Planetiler Shortbread tilegen for comparison.
//!
//! Replaces `bench-planetiler.sh`. Auto-downloads JDK + Planetiler JAR via
//! `tools::ensure_planetiler`, primes source data on first run, then runs N
//! times with external wall-clock timing (best of N).

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;
use crate::tools;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    data_dir: &Path,
    scratch_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let pt = tools::ensure_planetiler(data_dir, project_root)?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let java_str = pt
        .java
        .to_str()
        .ok_or_else(|| DevError::Config("Java path is not valid UTF-8".into()))?;

    let jar_str = pt.planetiler_jar.display().to_string();

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let heap_mb = std::cmp::max((file_mb as i64) * 2, 2048);
    let heap_arg = format!("-Xmx{heap_mb}m");

    std::fs::create_dir_all(scratch_dir)?;
    let output_path = scratch_dir.join("planetiler-bench.pmtiles");
    let output_str = output_path.display().to_string();

    let tmpdir = scratch_dir.join("planetiler_tmp");
    let tmpdir_str = tmpdir.display().to_string();

    // Prime Planetiler source data (ocean + natural earth) on first run.
    let sources_dir = data_dir.join("sources");
    if !sources_dir.exists() {
        output::bench_msg("priming Planetiler data (first-time download of ocean + natural earth)");

        let osm_arg = format!("--osm-path={pbf_str}");
        let out_arg = format!("--output={output_str}");
        let tmp_arg = format!("--tmpdir={tmpdir_str}");

        let prime_args: Vec<&str> = vec![
            &heap_arg,
            "-jar",
            &jar_str,
            "shortbread",
            "--force",
            "--download",
            "--area=extract",
            &osm_arg,
            &out_arg,
            "--nodemap-type=sparsearray",
            &tmp_arg,
        ];

        let captured = output::run_captured(java_str, &prime_args, project_root)?;
        captured.check_success(java_str)?;
        output::bench_msg("Planetiler data primed");
    }

    output::bench_msg(&format!(
        "Planetiler Shortbread: {basename} ({file_mb:.0} MB), {runs} run(s), heap {heap_mb}m"
    ));

    // Build the args that get passed to harness.run_external().
    let osm_arg = format!("--osm-path={pbf_str}");
    let out_arg = format!("--output={output_str}");
    let tmp_arg = format!("--tmpdir={tmpdir_str}");

    let args_owned: Vec<String> = vec![
        heap_arg,
        "-jar".into(),
        jar_str,
        "shortbread".into(),
        "--force".into(),
        "--download".into(),
        "--area=extract".into(),
        osm_arg,
        out_arg,
        "--nodemap-type=sparsearray".into(),
        tmp_arg,
        "--maxzoom=14".into(),
    ];
    let args_refs: Vec<&str> = args_owned.iter().map(String::as_str).collect();

    let config = BenchConfig {
        // `planetiler` comparison baseline, shortbread profile. Heap
        // size is a runtime observation (derived from input size, not
        // a user-provided flag) so it stays in metadata.
        command: "planetiler".into(),
        mode: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: crate::build::CargoProfile::Java,
        runs,
        cli_args: Some(crate::harness::format_cli_args(
            &pt.java.display().to_string(),
            &args_refs,
        )),
        brokkr_args: None,
        metadata: vec![KvPair::int("meta.heap_mb", heap_mb)],
    };

    harness.run_external(&config, &pt.java, &args_refs, project_root)?;

    // Clean up output file.
    std::fs::remove_file(&output_path).ok();

    Ok(())
}
