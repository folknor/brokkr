//! Benchmark: Planetiler Shortbread tilegen for comparison.
//!
//! Replaces `bench-planetiler.sh`. Auto-downloads JDK + Planetiler JAR via
//! `tools::ensure_planetiler`, primes source data on first run, then runs N
//! times with external wall-clock timing (best of N).

use std::path::Path;

use crate::clean::layout::ElivagarLayout;
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
    layout: &ElivagarLayout<'_>,
    project_root: &Path,
) -> Result<(), DevError> {
    let invocation = prepare(pbf_path, file_mb, runs, layout, project_root, false)?;
    let args: Vec<&str> = invocation.args.iter().map(String::as_str).collect();
    harness.run_external_prepared(
        &invocation.config,
        &invocation.program,
        &args,
        project_root,
        &[],
        || invocation.prepare_sample(),
    )?;
    invocation.cleanup();
    Ok(())
}

/// Run planetiler once with `--download` and mark the sources primed.
fn prime_sources(
    java: &str,
    prime_args: &[&str],
    project_root: &Path,
    sources_dir: &Path,
    primed_marker: &Path,
) -> Result<(), DevError> {
    let captured = output::run_captured(java, prime_args, project_root)?;
    captured.check_success(java)?;
    // A successful `--download` run that left `sources_dir` empty put its
    // sources somewhere else: planetiler did not honour `--download_dir`.
    // Marking it primed would send every timed run to the network, or
    // measure against sources it cannot find.
    let populated = std::fs::read_dir(sources_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    if !populated {
        return Err(DevError::Config(format!(
            "planetiler primed successfully but wrote nothing to {} - \
             it did not honour `--download_dir`; check the flag name \
             against the planetiler release in use",
            sources_dir.display()
        )));
    }
    std::fs::write(primed_marker, b"")?;
    output::bench_msg("Planetiler data primed");
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(crate) fn prepare(
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    layout: &ElivagarLayout<'_>,
    project_root: &Path,
    dry_run: bool,
) -> Result<crate::measure::runner::ExternalInvocation, DevError> {
    let data_dir = layout.data_dir();
    let scratch_dir = layout.scratch();
    let pt = if dry_run {
        tools::PlanetilerTools {
            java: data_dir.join("jdk/bin/java"),
            planetiler_jar: data_dir.join("planetiler.jar"),
            bench_class_dir: data_dir.join("planetiler-bench-classes"),
        }
    } else {
        tools::ensure_planetiler(data_dir, project_root)?
    };

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
    let heap_mb = std::cmp::max(
        (file_mb as i64) * super::constants::PLANETILER_HEAP_FILE_MULTIPLIER,
        super::constants::PLANETILER_MIN_HEAP_MB,
    );
    let heap_arg = format!("-Xmx{heap_mb}m");

    if !dry_run {
        std::fs::create_dir_all(scratch_dir)?;
    }
    let output_path = layout.planetiler_output();
    let output_str = output_path.display().to_string();

    let tmpdir = layout.planetiler_tmp();
    let tmpdir_str = tmpdir.display().to_string();

    // Where Planetiler keeps its source downloads (ocean + natural earth),
    // passed explicitly on every invocation. Planetiler's own default is
    // `data/sources` relative to its cwd (the project root), which matches the
    // `data_dir/sources` this check looks at only while the host's data dir
    // happens to be `<project>/data`.
    //
    // The flag name `download_dir` was written from memory of planetiler's
    // `Planetiler.java` (`arguments.file("download_dir", ...)`) and has not
    // been checked against a planetiler checkout - none is available here.
    // planetiler's `Arguments` looks keys up rather than validating the
    // command line, so a wrong name may well pass silently; the post-priming
    // check below is what catches it either way.
    let sources_dir = layout.planetiler_sources();
    let download_dir_arg = format!("--download_dir={}", sources_dir.display());
    // Written only after a priming run succeeds. The bare directory is not
    // evidence of anything: an interrupted download leaves it behind, half
    // full, and every later run would then measure against partial sources.
    let primed_marker = layout.planetiler_primed_marker();

    // Prime Planetiler source data on first run - the only invocation that
    // passes `--download`, so network time never lands inside a timed run.
    if !dry_run && !primed_marker.exists() {
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
            &download_dir_arg,
            "--area=extract",
            &osm_arg,
            &out_arg,
            "--nodemap-type=sparsearray",
            &tmp_arg,
        ];
        prime_sources(
            java_str,
            &prime_args,
            project_root,
            &sources_dir,
            &primed_marker,
        )?;
    }

    if !dry_run {
        output::bench_msg(&format!(
            "Planetiler Shortbread: {basename} ({file_mb:.0} MB), {runs} run(s), heap {heap_mb}m"
        ));
    }

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
        // No `--download`: sources were primed above. With it, Planetiler
        // re-checks (and on a stale source re-fetches) over the network inside
        // the measured wall.
        download_dir_arg,
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
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        external_build: Some(crate::build::BuildKind::Java),
        runs,
        cli_args: Some(crate::harness::format_cli_args(
            &pt.java.display().to_string(),
            &args_refs,
        )),
        metadata: vec![KvPair::int("meta.heap_mb", heap_mb)],
    };

    Ok(crate::measure::runner::ExternalInvocation {
        program: pt.java,
        args: args_owned,
        config,
        output_files: vec![output_path],
        scratch_dirs: vec![tmpdir],
    })
}
