//! Benchmark: Planetiler Java PBF read performance.

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;
use crate::tools;

struct ParsedResult {
    mode: String,
    elapsed_ms: i64,
    kv: Vec<KvPair>,
}

fn parse_planetiler_output(stderr: &str) -> Vec<ParsedResult> {
    let mut results = Vec::new();

    for block in stderr.split("---") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }

        let mut mode: Option<String> = None;
        let mut elapsed_ms: Option<i64> = None;
        let mut nodes: Option<i64> = None;
        let mut ways: Option<i64> = None;
        let mut relations: Option<i64> = None;

        for line in block.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };

            match key {
                "mode" => mode = Some(value.to_owned()),
                "elapsed_ms" => elapsed_ms = value.parse().ok(),
                "nodes" => nodes = value.parse().ok(),
                "ways" => ways = value.parse().ok(),
                "relations" => relations = value.parse().ok(),
                _ => {}
            }
        }

        if let (Some(mode), Some(elapsed_ms)) = (mode, elapsed_ms) {
            let kv = vec![
                KvPair::int("nodes", nodes.unwrap_or(0)),
                KvPair::int("ways", ways.unwrap_or(0)),
                KvPair::int("relations", relations.unwrap_or(0)),
            ];

            results.push(ParsedResult {
                mode,
                elapsed_ms,
                kv,
            });
        }
    }

    results
}

fn run_planetiler_subprocess(
    java: &Path,
    classpath: &str,
    pbf_path: &Path,
    heap_mb: i64,
    runs: usize,
    project_root: &Path,
) -> Result<Vec<ParsedResult>, DevError> {
    let heap_arg = format!("-Xmx{heap_mb}m");
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;
    let runs_str = runs.to_string();
    let java_str = java
        .to_str()
        .ok_or_else(|| DevError::Config("Java path is not valid UTF-8".into()))?;

    let args: Vec<&str> = vec![
        &heap_arg,
        "-cp",
        classpath,
        "BenchPbfRead",
        pbf_str,
        &runs_str,
    ];

    let captured = output::run_captured(java_str, &args, project_root)?;

    captured.check_success(java_str)?;

    let stderr = String::from_utf8_lossy(&captured.stderr);
    Ok(parse_planetiler_output(&stderr))
}

pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    data_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let pt = tools::ensure_planetiler(data_dir, project_root)?;

    #[allow(clippy::cast_possible_truncation)]
    let heap_mb = std::cmp::max((file_mb as i64) * 2, 2048);
    let classpath = format!(
        "{}:{}",
        pt.planetiler_jar.display(),
        pt.bench_class_dir.display()
    );

    let (basename, _) = super::path_strs(pbf_path)?;

    output::bench_msg("running planetiler benchmark");

    let results =
        run_planetiler_subprocess(&pt.java, &classpath, pbf_path, heap_mb, runs, project_root)?;

    if results.is_empty() {
        return Err(DevError::Build(
            "no results from planetiler benchmark".into(),
        ));
    }

    let variant_names: Vec<&str> = results.iter().map(|r| r.mode.as_str()).collect();

    crate::harness::run_variants("mode", &variant_names, |mode| {
        let result = results.iter().find(|r| r.mode == mode).expect("mode exists in results");

        let config = BenchConfig {
            command: "bench planetiler".into(),
            variant: Some(mode.into()),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: "java".into(),
            runs: 1,
            cli_args: None,
            metadata: vec![KvPair::int("meta.heap_mb", heap_mb)],
        };

        harness.run_internal(&config, |_| {
            Ok(BenchResult {
                elapsed_ms: result.elapsed_ms,
                kv: result.kv.clone(),
                distribution: None,
                hotpath: None,
            })
        }).map(|_| ())
    })
}
