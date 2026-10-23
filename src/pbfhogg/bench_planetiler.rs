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

fn parse_planetiler_output(stderr: &str) -> Result<Vec<ParsedResult>, DevError> {
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
                "elapsed_ms" => elapsed_ms = Some(parse_metric(key, value)?),
                "nodes" => nodes = Some(parse_metric(key, value)?),
                "ways" => ways = Some(parse_metric(key, value)?),
                "relations" => relations = Some(parse_metric(key, value)?),
                _ => {}
            }
        }

        let Some(mode) = mode else { continue };
        let elapsed_ms = required_metric("elapsed_ms", elapsed_ms)?;
        let nodes = required_metric("nodes", nodes)?;
        let ways = required_metric("ways", ways)?;
        let relations = required_metric("relations", relations)?;
        let kv = vec![
            KvPair::int("nodes", nodes),
            KvPair::int("ways", ways),
            KvPair::int("relations", relations),
        ];

        results.push(ParsedResult {
            mode,
            elapsed_ms,
            kv,
        });
    }

    Ok(results)
}

fn parse_metric(key: &str, value: &str) -> Result<i64, DevError> {
    let parsed = value.parse::<i64>().map_err(|_| {
        DevError::Build(format!(
            "planetiler benchmark emitted invalid {key}: {value}"
        ))
    })?;
    if parsed < 0 {
        return Err(DevError::Build(format!(
            "planetiler benchmark emitted negative {key}: {parsed}"
        )));
    }
    Ok(parsed)
}

fn required_metric(key: &str, value: Option<i64>) -> Result<i64, DevError> {
    value.ok_or_else(|| DevError::Build(format!("planetiler benchmark result is missing {key}")))
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
    let pt = tools::ensure_planetiler(data_dir, project_root)?;

    #[allow(clippy::cast_possible_truncation)]
    let heap_mb = std::cmp::max((file_mb as i64) * 2, 2048);
    let classpath = format!(
        "{}:{}",
        pt.planetiler_jar.display(),
        pt.bench_class_dir.display()
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

    let variant_names: Vec<&str> = results.iter().map(|r| r.mode.as_str()).collect();

    crate::bench_variants::run_variants("mode", &variant_names, |mode| {
        let result = results
            .iter()
            .find(|r| r.mode == mode)
            .expect("mode exists in results");

        let config = BenchConfig {
            // Planetiler emits multiple mode results per Java subprocess
            // run (elements/blobs/features/…). Neither cli_args nor
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
    })
}
