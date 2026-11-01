//! Benchmark: Planetiler Java PBF read performance.

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::measure::{Invocation, ResolvedExecutable};
use crate::tools;

struct ParsedResult {
    mode: String,
    elapsed_ms: i64,
    kv: Vec<KvPair>,
}

/// The bench's result blocks (grammar in `super::baseline_blocks`), each
/// required to carry its element counts.
fn parse_planetiler_output(stderr: &str) -> Result<Vec<ParsedResult>, DevError> {
    const TOOL: &str = "planetiler";
    super::baseline_blocks::parse(TOOL, stderr)?
        .into_iter()
        .map(|block| {
            let kv = vec![
                KvPair::int("nodes", block.required_count(TOOL, "nodes")?),
                KvPair::int("ways", block.required_count(TOOL, "ways")?),
                KvPair::int("relations", block.required_count(TOOL, "relations")?),
            ];
            Ok(ParsedResult {
                mode: block.mode,
                elapsed_ms: block.elapsed_ms,
                kv,
            })
        })
        .collect()
}

fn run_planetiler_subprocess(
    java: &ResolvedExecutable,
    classpath: &str,
    pbf_path: &Path,
    heap_mb: i64,
    runs: usize,
    project_root: &Path,
) -> Result<Vec<ParsedResult>, DevError> {
    let pbf_str = crate::utf8_path::path_str(pbf_path)?;
    let invocation = Invocation::resolved(
        java,
        vec![
            format!("-Xmx{heap_mb}m"),
            "-cp".into(),
            classpath.to_owned(),
            "BenchPbfRead".into(),
            pbf_str.to_owned(),
            runs.to_string(),
        ],
    );

    let captured = invocation.capture_in(project_root)?;
    invocation.check(&captured)?;

    let stderr = String::from_utf8_lossy(&captured.stderr);
    parse_planetiler_output(&stderr)
}

pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    data_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let pt = tools::ensure_planetiler_runtime(data_dir)?;
    // `project_root` is the suite's build root, so a `--commit` run compiles
    // the commit's baseline source.
    let bench_class_dir = tools::planetiler_bench_classes(data_dir, project_root, &pt)?;

    #[allow(clippy::cast_possible_truncation)]
    let heap_mb = std::cmp::max((file_mb as i64) * 2, 2048);
    let classpath = format!(
        "{}:{}",
        pt.planetiler_jar.display(),
        bench_class_dir.display()
    );

    let (basename, _) = crate::utf8_path::path_strings(pbf_path)?;

    let results = run_planetiler_subprocess(
        &pt.java_executable(),
        &classpath,
        pbf_path,
        heap_mb,
        runs,
        project_root,
    )?;

    if results.is_empty() {
        return Err(DevError::Build(
            "no results from planetiler benchmark".into(),
        ));
    }

    crate::bench_variants::run_variants_of(
        "mode",
        &results,
        |result| result.mode.as_str(),
        |result| {
            let mode = &result.mode;
            let config = BenchConfig {
                // Planetiler emits several mode results per Java subprocess
                // run (sequential and parallel read). Neither cli_args nor
                // brokkr_args distinguishes them, so the mode goes in the
                // command column as `planetiler-<mode>`. Query with
                // `--command planetiler` (substring) for the whole suite.
                command: format!("planetiler-{mode}"),
                input_file: Some(basename.clone()),
                input_mb: Some(file_mb),
                cargo_features: None,
                external_build: Some(crate::build::BuildKind::Java),
                runs: 1,
                cli_args: None,
                metadata: vec![KvPair::int("meta.heap_mb", heap_mb)],
                arm_qualifier: None,
            };

            harness
                .run_internal(&config, |_| {
                    Ok(BenchResult {
                        elapsed_ms: result.elapsed_ms,
                        elapsed_us: None,
                        kv: result.kv.clone(),
                        iterations: Vec::new(),
                        distribution: None,
                        hotpath: None,
                    })
                })
                .map(|_| ())
        },
    )
}
