mod build;
mod config;
mod db;
mod env;
mod error;
mod git;
mod harness;
mod lockfile;
mod output;
mod pbfhogg;
mod elivagar;
#[allow(dead_code)]
mod preflight;
mod project;
mod tools;

use std::path::{Path, PathBuf};
use std::process;

use clap::{Parser, Subcommand};

use error::DevError;
use project::Project;

#[derive(Parser)]
#[command(name = "dev", about = "Shared development tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run clippy + tests
    Check {
        /// Extra arguments passed to cargo test
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show environment information
    Env,
    /// Build and run the project binary
    Run {
        /// Arguments passed to the binary
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Query benchmark results
    Results {
        /// Show results for a specific commit (prefix match)
        #[arg(long)]
        commit: Option<String>,

        /// Compare two commits side-by-side
        #[arg(long, num_args = 2, value_names = ["COMMIT_A", "COMMIT_B"])]
        compare: Option<Vec<String>>,

        /// Filter by command name (e.g. "bench read", "bench merge")
        #[arg(long)]
        command: Option<String>,

        /// Filter by variant (e.g. "buffered+zlib", "pipelined")
        #[arg(long)]
        variant: Option<String>,

        /// Maximum number of results to show
        #[arg(long, short = 'n', default_value = "20")]
        limit: usize,
    },
    /// Clean build artifacts and scratch data
    Clean,
    /// Run benchmarks
    Bench {
        #[command(subcommand)]
        bench: BenchCommand,
    },
    /// Cross-validate pbfhogg output against reference tools
    Verify {
        #[command(subcommand)]
        verify: VerifyCommand,
    },
    /// Run hotpath profiling (timing or allocation instrumentation)
    Hotpath {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Explicit OSC diff file path (overrides --dataset)
        #[arg(long)]
        osc: Option<String>,

        /// Run allocation profiling instead of timing
        #[arg(long)]
        alloc: bool,

        /// Number of runs (default: 1 for profiling)
        #[arg(long, default_value = "1")]
        runs: usize,
    },
    /// Run two-pass profiling (timing + allocation) for a dataset
    Profile {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Explicit OSC diff file path (overrides --dataset)
        #[arg(long)]
        osc: Option<String>,

        /// Profiling tool: perf or samply (elivagar only)
        #[arg(long)]
        tool: Option<String>,
    },
    /// Download a region dataset from Geofabrik
    Download {
        /// Region name (malta, greater-london, switzerland, norway, japan, denmark, germany, north-america)
        region: String,

        /// URL for the OSC diff file
        #[arg(long)]
        osc_url: Option<String>,
    },
    /// Compare feature counts between two PMTiles archives (elivagar)
    CompareTiles {
        /// First PMTiles file
        file_a: String,
        /// Second PMTiles file
        file_b: String,
        /// Sample size per zoom level
        #[arg(long)]
        sample: Option<usize>,
    },
    /// Download ocean shapefiles (elivagar)
    DownloadOcean,
}

#[derive(Subcommand)]
enum BenchCommand {
    /// Benchmark CLI commands (external timing)
    Commands {
        #[arg(default_value = "all")]
        command: String,
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Benchmark extract strategies (simple/complete/smart)
    Extract {
        #[arg(long, default_value = "japan")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long)]
        bbox: Option<String>,
        #[arg(long, default_value = "simple,complete,smart")]
        strategies: String,
    },
    /// Benchmark allocators (default/jemalloc/mimalloc) via check-refs
    Allocator {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Benchmark indexed vs non-indexed PBF performance
    BlobFilter {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf_indexed: Option<String>,
        #[arg(long)]
        pbf_raw: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Benchmark Planetiler Java PBF read performance
    Planetiler {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Read benchmark (5 modes)
    Read {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long, default_value = "sequential,parallel,pipelined,mmap,blobreader")]
        modes: String,
    },
    /// Write benchmark (sync + pipelined x compression)
    Write {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long, default_value = "none,zlib:6,zstd:3")]
        compression: String,
    },
    /// Merge benchmark (I/O modes x compression)
    Merge {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        osc: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
        #[arg(long)]
        uring: bool,
        #[arg(long, default_value = "zlib,none")]
        compression: String,
    },
    /// Run full benchmark suite (commands + baselines)
    All {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },

    // ----- Elivagar bench variants -----

    /// Elivagar: full pipeline benchmark
    #[command(name = "self")]
    ElivSelf {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "1")]
        runs: usize,
        /// Resume from checkpoint: ocean or sort
        #[arg(long)]
        skip_to: Option<String>,
        /// Skip ocean processing
        #[arg(long)]
        no_ocean: bool,
        /// Gzip compression level 0-10
        #[arg(long)]
        compression_level: Option<u32>,
    },
    /// Elivagar: SortedNodeStore benchmark
    NodeStore {
        /// Nodes in millions
        #[arg(long, default_value = "50")]
        nodes: usize,
        #[arg(long, default_value = "5")]
        runs: usize,
    },
    /// Elivagar: PMTiles writer benchmark
    Pmtiles {
        /// Number of tiles
        #[arg(long, default_value = "500000")]
        tiles: usize,
        #[arg(long, default_value = "5")]
        runs: usize,
    },
    /// Elivagar: Planetiler comparison benchmark
    ElivPlanetiler {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Elivagar: Tilemaker comparison benchmark
    Tilemaker {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Elivagar: full benchmark suite
    ElivAll {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },
}

#[derive(Subcommand)]
enum VerifyCommand {
    /// Cross-validate sort against osmium sort
    Sort {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate cat (type filters) against osmium cat
    Cat {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate extract (bbox strategies) against osmium extract
    Extract {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        bbox: Option<String>,
    },
    /// Cross-validate tags-filter against osmium tags-filter
    TagsFilter {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate getid/removeid against osmium getid
    GetidRemoveid {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate add-locations-to-ways against osmium
    AddLocationsToWays {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate check-refs against osmium check-refs
    CheckRefs {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate merge against osmium/osmosis/osmconvert
    Merge {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        osc: Option<String>,
    },
    /// Cross-validate derive-changes roundtrip against osmium
    DeriveChanges {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        osc: Option<String>,
    },
    /// Cross-validate diff summary against osmium diff
    Diff {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        osc: Option<String>,
    },
    /// Run all verify commands sequentially
    All {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long)]
        osc: Option<String>,
        #[arg(long)]
        bbox: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = run(cli);
    if let Err(e) = result {
        output::error(&e.to_string());
        process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), DevError> {
    let (project, project_root) = project::detect()?;

    match cli.command {
        Command::Check { args } => cmd_check(project, &project_root, &args),
        Command::Env => cmd_env(project, &project_root),
        Command::Run { args } => cmd_run(project, &project_root, &args),
        Command::Results {
            commit,
            compare,
            command,
            variant,
            limit,
        } => cmd_results(&project_root, commit, compare, command, variant, limit),
        Command::Clean => cmd_clean(project, &project_root),
        Command::Bench { bench } => cmd_bench(project, &project_root, bench),
        Command::Verify { verify } => cmd_verify(project, &project_root, verify),
        Command::Hotpath {
            dataset,
            pbf,
            osc,
            alloc,
            runs,
        } => cmd_hotpath(project, &project_root, dataset, pbf, osc, alloc, runs),
        Command::Profile { dataset, pbf, osc, tool } => {
            cmd_profile(project, &project_root, dataset, pbf, osc, tool)
        }
        Command::Download { region, osc_url } => {
            cmd_download(project, &project_root, region, osc_url)
        }
        Command::CompareTiles { file_a, file_b, sample } => {
            cmd_compare_tiles(project, &project_root, &file_a, &file_b, sample)
        }
        Command::DownloadOcean => cmd_download_ocean(project, &project_root),
    }
}

// ---------------------------------------------------------------------------
// Bootstrap helpers
// ---------------------------------------------------------------------------

/// Resolve project info (target_dir) using cargo metadata.
fn bootstrap(project_root: &Path) -> Result<build::ProjectInfo, DevError> {
    build::project_info()
}

/// Load config and resolve paths for the current host.
fn bootstrap_config(
    project_root: &Path,
    target_dir: &Path,
) -> Result<(config::DevConfig, config::ResolvedPaths), DevError> {
    let hostname = config::hostname()?;
    let dev_config = config::load(project_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, project_root, target_dir);
    Ok((dev_config, paths))
}

// ---------------------------------------------------------------------------
// Shared commands
// ---------------------------------------------------------------------------

fn cmd_check(project: Project, project_root: &Path, extra_args: &[String]) -> Result<(), DevError> {
    run_clippy(project_root)?;
    run_tests(project, project_root, extra_args)?;
    output::result_msg("check passed");
    Ok(())
}

fn run_clippy(project_root: &Path) -> Result<(), DevError> {
    output::run_msg("cargo clippy --all-targets");

    let captured = output::run_captured("cargo", &["clippy", "--all-targets"], project_root)?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        output::error(&stderr);
        return Err(DevError::Build("clippy failed".into()));
    }

    Ok(())
}

fn run_tests(
    project: Project,
    project_root: &Path,
    extra_args: &[String],
) -> Result<(), DevError> {
    let mut args = vec!["test"];
    let extra_refs: Vec<&str> = extra_args.iter().map(String::as_str).collect();
    args.extend_from_slice(&extra_refs);

    output::run_msg(&format!("cargo {}", args.join(" ")));

    // Nidhogg tests need CARGO_TARGET_TMPDIR set.
    let env: Vec<(&str, &str)> = match project {
        Project::Nidhogg => vec![("CARGO_TARGET_TMPDIR", "target/tmp")],
        _ => vec![],
    };

    let captured = if env.is_empty() {
        output::run_captured("cargo", &args, project_root)?
    } else {
        output::run_captured_with_env("cargo", &args, project_root, &env)?
    };

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        output::error(&stderr);
        return Err(DevError::Build("tests failed".into()));
    }

    Ok(())
}

fn cmd_env(project: Project, project_root: &Path) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;

    let info = env::collect(&dev_config, &paths, project);
    env::print(&info);
    Ok(())
}

fn cmd_run(project: Project, project_root: &Path, args: &[String]) -> Result<(), DevError> {
    let package = project.cli_package();
    let binary = build::cargo_build(
        &build::BuildConfig::release(package),
        project_root,
    )?;

    output::run_msg(&format!("{} {}", binary.display(), args.join(" ")));

    let code = output::run_passthrough(&binary, args)?;
    if code != 0 {
        process::exit(code);
    }
    Ok(())
}

fn cmd_results(
    project_root: &Path,
    commit: Option<String>,
    compare: Option<Vec<String>>,
    command: Option<String>,
    variant: Option<String>,
    limit: usize,
) -> Result<(), DevError> {
    let db_path = results_db_path(project_root);

    if !db_path.exists() {
        output::result_msg("no results yet (run a benchmark first)");
        return Ok(());
    }

    let results_db = db::ResultsDb::open(&db_path)?;

    if let Some(commits) = compare {
        let commit_a = commits.first().map_or("", String::as_str);
        let commit_b = commits.get(1).map_or("", String::as_str);
        let (rows_a, rows_b) = results_db.query_compare(commit_a, commit_b)?;
        let table = db::format_compare(commit_a, &rows_a, commit_b, &rows_b);
        println!("{table}");
    } else {
        let filter = db::QueryFilter {
            commit,
            command,
            variant,
            limit,
        };
        let rows = results_db.query(&filter)?;
        if rows.is_empty() {
            output::result_msg("no matching results");
        } else {
            let table = db::format_table(&rows);
            println!("{table}");
        }
    }

    Ok(())
}

fn cmd_clean(project: Project, project_root: &Path) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (_, paths) = bootstrap_config(project_root, &pi.target_dir)?;

    // Clean verify output (pbfhogg only).
    let verify_dir = paths.target_dir.join("verify");
    if verify_dir.exists() {
        std::fs::remove_dir_all(&verify_dir)?;
        output::run_msg("removed verify output");
    }

    // Clean scratch temp files.
    if paths.scratch_dir.exists() {
        if project == Project::Elivagar {
            // Elivagar scratch is tilegen_tmp — remove all contents.
            std::fs::remove_dir_all(&paths.scratch_dir)?;
            std::fs::create_dir_all(&paths.scratch_dir)?;
            output::run_msg("cleaned tilegen_tmp");
        } else {
            let mut removed = 0u32;
            if let Ok(entries) = std::fs::read_dir(&paths.scratch_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("pbf") {
                        let _ = std::fs::remove_file(&path);
                        removed += 1;
                    }
                }
            }
            if removed > 0 {
                output::run_msg(&format!("removed {removed} scratch file(s)"));
            }
        }
    }

    output::result_msg("clean done");
    Ok(())
}

// ---------------------------------------------------------------------------
// Path resolution helpers
// ---------------------------------------------------------------------------

/// Resolve the PBF path from --pbf or --dataset.
fn resolve_pbf_path(
    pbf: &Option<String>,
    dataset: &str,
    dev_config: &config::DevConfig,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, DevError> {
    let path = match pbf {
        Some(p) => PathBuf::from(p),
        None => {
            let ds = dev_config.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            let pbf_file = ds.pbf.as_ref().ok_or_else(|| {
                DevError::Config(format!("dataset '{dataset}' has no pbf configured"))
            })?;
            paths.data_dir.join(pbf_file)
        }
    };

    if !path.exists() {
        return Err(DevError::Config(format!(
            "PBF file not found: {}",
            path.display()
        )));
    }

    Ok(path)
}

/// Resolve the OSC path from --osc or --dataset.
fn resolve_osc_path(
    osc: &Option<String>,
    dataset: &str,
    dev_config: &config::DevConfig,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, DevError> {
    let path = match osc {
        Some(p) => PathBuf::from(p),
        None => {
            let ds = dev_config.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            let osc_file = ds.osc.as_ref().ok_or_else(|| {
                DevError::Config(format!(
                    "dataset '{dataset}' has no osc file configured"
                ))
            })?;
            paths.data_dir.join(osc_file)
        }
    };

    if !path.exists() {
        return Err(DevError::Config(format!(
            "OSC file not found: {}",
            path.display()
        )));
    }

    Ok(path)
}

/// Resolve the bbox from --bbox or dataset config.
fn resolve_bbox(
    bbox: &Option<String>,
    dataset: &str,
    dev_config: &config::DevConfig,
) -> Result<String, DevError> {
    if let Some(b) = bbox {
        return Ok(b.clone());
    }

    let ds = dev_config.datasets.get(dataset).ok_or_else(|| {
        DevError::Config(format!("unknown dataset: {dataset}"))
    })?;

    ds.bbox.clone().ok_or_else(|| {
        DevError::Config(format!(
            "dataset '{dataset}' has no bbox configured (use --bbox)"
        ))
    })
}

/// Resolve the non-indexed (raw) PBF path from --pbf-raw or dataset config.
fn resolve_raw_pbf_path(
    pbf_raw: &Option<String>,
    dataset: &str,
    dev_config: &config::DevConfig,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, DevError> {
    let path = match pbf_raw {
        Some(p) => PathBuf::from(p),
        None => {
            let ds = dev_config.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            let raw_file = ds.pbf_raw.as_ref().ok_or_else(|| {
                DevError::Config(format!(
                    "dataset '{dataset}' has no pbf_raw configured (use --pbf-raw)"
                ))
            })?;
            paths.data_dir.join(raw_file)
        }
    };

    if !path.exists() {
        return Err(DevError::Config(format!(
            "raw PBF file not found: {}",
            path.display()
        )));
    }

    Ok(path)
}

/// Get file size in MB (decimal, consistent with bench scripts).
fn file_size_mb(path: &Path) -> f64 {
    std::fs::metadata(path)
        .map(|m| m.len() as f64 / 1_000_000.0)
        .unwrap_or(0.0)
}

/// Path to the results database for the current project.
fn results_db_path(project_root: &Path) -> PathBuf {
    project_root.join(".dev").join("results.db")
}

// ---------------------------------------------------------------------------
// Bench commands (pbfhogg-specific)
// ---------------------------------------------------------------------------

fn cmd_bench(project: Project, project_root: &Path, bench: BenchCommand) -> Result<(), DevError> {
    match bench {
        // ----- pbfhogg bench variants -----
        BenchCommand::Commands { command, dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench commands")?;
            cmd_bench_commands(project, project_root, command, dataset, pbf, runs)
        }
        BenchCommand::Extract { dataset, pbf, runs, bbox, strategies } => {
            project::require(project, Project::Pbfhogg, "bench extract")?;
            cmd_bench_extract(project, project_root, dataset, pbf, runs, bbox, strategies)
        }
        BenchCommand::Allocator { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench allocator")?;
            cmd_bench_allocator(project, project_root, dataset, pbf, runs)
        }
        BenchCommand::BlobFilter { dataset, pbf_indexed, pbf_raw, runs } => {
            project::require(project, Project::Pbfhogg, "bench blob-filter")?;
            cmd_bench_blob_filter(project, project_root, dataset, pbf_indexed, pbf_raw, runs)
        }
        BenchCommand::Planetiler { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench planetiler")?;
            cmd_bench_planetiler(project, project_root, dataset, pbf, runs)
        }
        BenchCommand::Read { dataset, pbf, runs, modes } => {
            project::require(project, Project::Pbfhogg, "bench read")?;
            cmd_bench_read(project, project_root, dataset, pbf, runs, modes)
        }
        BenchCommand::Write { dataset, pbf, runs, compression } => {
            project::require(project, Project::Pbfhogg, "bench write")?;
            cmd_bench_write(project, project_root, dataset, pbf, runs, compression)
        }
        BenchCommand::Merge { dataset, pbf, osc, runs, uring, compression } => {
            project::require(project, Project::Pbfhogg, "bench merge")?;
            cmd_bench_merge(project, project_root, dataset, pbf, osc, runs, uring, compression)
        }
        BenchCommand::All { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench all")?;
            cmd_bench_all(project, project_root, dataset, pbf, runs)
        }

        // ----- elivagar bench variants -----
        BenchCommand::ElivSelf { dataset, pbf, runs, skip_to, no_ocean, compression_level } => {
            project::require(project, Project::Elivagar, "bench self")?;
            cmd_bench_eliv_self(project, project_root, dataset, pbf, runs, skip_to, no_ocean, compression_level)
        }
        BenchCommand::NodeStore { nodes, runs } => {
            project::require(project, Project::Elivagar, "bench node-store")?;
            let pi = bootstrap(project_root)?;
            elivagar::bench_node_store::run(&pi.target_dir, project_root, Some(nodes), Some(runs))
        }
        BenchCommand::Pmtiles { tiles, runs } => {
            project::require(project, Project::Elivagar, "bench pmtiles")?;
            let pi = bootstrap(project_root)?;
            elivagar::bench_pmtiles::run(&pi.target_dir, project_root, Some(tiles), Some(runs))
        }
        BenchCommand::ElivPlanetiler { dataset, pbf, runs } => {
            project::require(project, Project::Elivagar, "bench eliv-planetiler")?;
            cmd_bench_eliv_planetiler(project, project_root, dataset, pbf, runs)
        }
        BenchCommand::Tilemaker { dataset: _, pbf: _, runs: _ } => {
            project::require(project, Project::Elivagar, "bench tilemaker")?;
            elivagar::bench_tilemaker::run()
        }
        BenchCommand::ElivAll { dataset, pbf, runs } => {
            project::require(project, Project::Elivagar, "bench eliv-all")?;
            cmd_bench_eliv_all(project, project_root, dataset, pbf, runs)
        }
    }
}

fn cmd_bench_commands(
    project: Project,
    project_root: &Path,
    command: String,
    dataset: String,
    pbf: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let commands = pbfhogg::bench_commands::parse_command(&command)?;
    let file_mb = file_size_mb(&pbf_path);
    let binary = build::cargo_build(&build::BuildConfig::release(Some("pbfhogg-cli")), project_root)?;
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    pbfhogg::bench_commands::run(&harness, &binary, &pbf_path, file_mb, runs, &commands, project_root)
}

fn cmd_bench_extract(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    runs: usize,
    bbox: Option<String>,
    strategies_str: String,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let bbox = resolve_bbox(&bbox, &dataset, &dev_config)?;
    let strategies = pbfhogg::bench_extract::parse_strategies(&strategies_str)?;
    let file_mb = file_size_mb(&pbf_path);
    let binary = build::cargo_build(&build::BuildConfig::release(Some("pbfhogg-cli")), project_root)?;
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    pbfhogg::bench_extract::run(&harness, &binary, &pbf_path, file_mb, runs, &bbox, &strategies, project_root)
}

fn cmd_bench_allocator(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let file_mb = file_size_mb(&pbf_path);
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    pbfhogg::bench_allocator::run(&harness, &pbf_path, file_mb, runs, project_root)
}

fn cmd_bench_blob_filter(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf_indexed: Option<String>,
    pbf_raw: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let indexed_path = resolve_pbf_path(&pbf_indexed, &dataset, &dev_config, &paths)?;
    let raw_path = resolve_raw_pbf_path(&pbf_raw, &dataset, &dev_config, &paths)?;
    let file_mb = file_size_mb(&indexed_path);
    let binary = build::cargo_build(&build::BuildConfig::release(Some("pbfhogg-cli")), project_root)?;
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    pbfhogg::bench_blob_filter::run(&harness, &binary, &indexed_path, &raw_path, file_mb, runs, project_root)
}

fn cmd_bench_planetiler(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let file_mb = file_size_mb(&pbf_path);
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    pbfhogg::bench_planetiler::run(&harness, &pbf_path, file_mb, runs, &paths.data_dir, project_root)
}

fn cmd_bench_read(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    runs: usize,
    modes_str: String,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let modes = pbfhogg::bench_read::parse_modes(&modes_str)?;
    let file_mb = file_size_mb(&pbf_path);
    let binary = build::cargo_build(&build::BuildConfig::release(Some("pbfhogg-cli")), project_root)?;
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    pbfhogg::bench_read::run(&harness, &binary, &pbf_path, file_mb, runs, &modes, project_root)
}

fn cmd_bench_write(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    runs: usize,
    compression_str: String,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let compressions = pbfhogg::bench_write::parse_compressions(&compression_str)?;
    let file_mb = file_size_mb(&pbf_path);
    let binary = build::cargo_build(&build::BuildConfig::release(Some("pbfhogg-cli")), project_root)?;
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    pbfhogg::bench_write::run(&harness, &binary, &pbf_path, file_mb, runs, &compressions, project_root)
}

#[allow(clippy::too_many_arguments)]
fn cmd_bench_merge(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    osc: Option<String>,
    runs: usize,
    uring: bool,
    compression_str: String,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;
    let compressions = pbfhogg::bench_merge::parse_compressions(&compression_str)?;
    let file_mb = file_size_mb(&pbf_path);

    if uring {
        pbfhogg::bench_merge::check_uring_preflight()?;
    }

    let binary = build::cargo_build(&build::BuildConfig::release(Some("pbfhogg-cli")), project_root)?;
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    pbfhogg::bench_merge::run(
        &harness,
        &binary,
        &pbf_path,
        &osc_path,
        file_mb,
        runs,
        &compressions,
        uring,
        &paths.scratch_dir,
        project_root,
    )
}

fn cmd_bench_all(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let file_mb = file_size_mb(&pbf_path);
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    pbfhogg::bench_all::run(&harness, &dev_config, &paths, project_root, &pbf_path, file_mb, runs, &dataset)
}

// ---------------------------------------------------------------------------
// Bench commands (elivagar-specific)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_bench_eliv_self(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    runs: usize,
    skip_to: Option<String>,
    no_ocean: bool,
    compression_level: Option<u32>,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let file_mb = file_size_mb(&pbf_path);
    let binary = build::cargo_build(&build::BuildConfig::release(None), project_root)?;
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    elivagar::bench_self::run(
        &harness,
        &binary,
        &pbf_path,
        file_mb,
        runs,
        &paths.data_dir,
        &paths.scratch_dir,
        project_root,
        skip_to.as_deref(),
        no_ocean,
        compression_level,
    )
}

fn cmd_bench_eliv_planetiler(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let file_mb = file_size_mb(&pbf_path);
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    elivagar::bench_planetiler::run(
        &harness,
        &pbf_path,
        file_mb,
        runs,
        &paths.data_dir,
        &paths.scratch_dir,
        project_root,
    )
}

fn cmd_bench_eliv_all(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let file_mb = file_size_mb(&pbf_path);
    let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
    elivagar::bench_all::run(
        &harness,
        &dev_config,
        &paths,
        project_root,
        &pbf_path,
        file_mb,
        runs,
        &paths.data_dir,
        &paths.scratch_dir,
    )
}

// ---------------------------------------------------------------------------
// Elivagar top-level commands
// ---------------------------------------------------------------------------

fn cmd_compare_tiles(
    project: Project,
    project_root: &Path,
    file_a: &str,
    file_b: &str,
    sample: Option<usize>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "compare-tiles")?;
    let pi = bootstrap(project_root)?;
    elivagar::compare_tiles::run(&pi.target_dir, project_root, file_a, file_b, sample)
}

fn cmd_download_ocean(project: Project, project_root: &Path) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "download-ocean")?;
    let pi = bootstrap(project_root)?;
    let (_, paths) = bootstrap_config(project_root, &pi.target_dir)?;
    elivagar::download_ocean::run(&paths.data_dir)
}

// ---------------------------------------------------------------------------
// Verify commands (pbfhogg-specific)
// ---------------------------------------------------------------------------

fn cmd_verify(project: Project, project_root: &Path, verify: VerifyCommand) -> Result<(), DevError> {
    project::require(project, Project::Pbfhogg, "verify")?;

    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;

    let harness = pbfhogg::verify::VerifyHarness::new(&paths, project_root, &pi.target_dir)?;

    match verify {
        VerifyCommand::Sort { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            pbfhogg::verify_sort::run(&harness, &pbf_path)
        }
        VerifyCommand::Cat { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            pbfhogg::verify_cat::run(&harness, &pbf_path)
        }
        VerifyCommand::Extract { dataset, pbf, bbox } => {
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            let bbox = resolve_bbox(&bbox, &dataset, &dev_config)?;
            pbfhogg::verify_extract::run(&harness, &pbf_path, &bbox)
        }
        VerifyCommand::TagsFilter { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            pbfhogg::verify_tags_filter::run(&harness, &pbf_path)
        }
        VerifyCommand::GetidRemoveid { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            pbfhogg::verify_getid_removeid::run(&harness, &pbf_path)
        }
        VerifyCommand::AddLocationsToWays { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            pbfhogg::verify_add_locations::run(&harness, &pbf_path)
        }
        VerifyCommand::CheckRefs { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            pbfhogg::verify_check_refs::run(&harness, &pbf_path)
        }
        VerifyCommand::Merge { dataset, pbf, osc } => {
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;
            let osmosis = match tools::ensure_osmosis(&paths.data_dir, project_root) {
                Ok(tools) => Some(tools),
                Err(e) => {
                    output::verify_msg(&format!("osmosis not available (non-fatal): {e}"));
                    None
                }
            };
            pbfhogg::verify_merge::run(&harness, &pbf_path, &osc_path, osmosis.as_ref())
        }
        VerifyCommand::DeriveChanges { dataset, pbf, osc } => {
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;
            pbfhogg::verify_derive_changes::run(&harness, &pbf_path, &osc_path)
        }
        VerifyCommand::Diff { dataset, pbf, osc } => {
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;
            pbfhogg::verify_diff::run(&harness, &pbf_path, &osc_path)
        }
        VerifyCommand::All { dataset, pbf, osc, bbox } => {
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            let osc_path = osc
                .as_ref()
                .map(|_| resolve_osc_path(&osc, &dataset, &dev_config, &paths))
                .transpose()?;
            let bbox_str = bbox
                .as_ref()
                .map(|_| resolve_bbox(&bbox, &dataset, &dev_config))
                .transpose()?;
            pbfhogg::verify_all::run(
                &harness,
                &pbf_path,
                osc_path.as_deref(),
                bbox_str.as_deref(),
                &paths.data_dir,
                project_root,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Hotpath / Profile / Download
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_hotpath(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    osc: Option<String>,
    alloc: bool,
    runs: usize,
) -> Result<(), DevError> {
    match project {
        Project::Elivagar => {
            let pi = bootstrap(project_root)?;
            let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            let feature = if alloc { "hotpath-alloc" } else { "hotpath" };
            let binary = build::cargo_build(
                &build::BuildConfig::release_with_features(None, &[feature]),
                project_root,
            )?;
            elivagar::hotpath::run(
                &binary,
                &pbf_path,
                &paths.data_dir,
                &paths.scratch_dir,
                alloc,
                project_root,
            )
        }
        _ => {
            project::require(project, Project::Pbfhogg, "hotpath")?;

            let pi = bootstrap(project_root)?;
            let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;
            let file_mb = file_size_mb(&pbf_path);

            let feature = if alloc { "hotpath-alloc" } else { "hotpath" };
            let binary = build::cargo_build(
                &build::BuildConfig::release_with_features(Some("pbfhogg-cli"), &[feature]),
                project_root,
            )?;

            // Try to get raw PBF path (optional).
            let pbf_raw_path = dev_config
                .datasets
                .get(&dataset)
                .and_then(|ds| ds.pbf_raw.as_ref())
                .map(|raw_file| paths.data_dir.join(raw_file))
                .filter(|p| p.exists());

            let harness = harness::BenchHarness::new(&dev_config, &paths, project_root, project)?;
            pbfhogg::hotpath::run(
                &harness,
                &binary,
                &pbf_path,
                pbf_raw_path.as_deref(),
                &osc_path,
                file_mb,
                runs,
                alloc,
                &paths.scratch_dir,
                project_root,
            )
        }
    }
}

fn cmd_profile(
    project: Project,
    project_root: &Path,
    dataset: String,
    pbf: Option<String>,
    osc: Option<String>,
    tool: Option<String>,
) -> Result<(), DevError> {
    match project {
        Project::Elivagar => {
            let tool_name = tool.as_deref().unwrap_or("perf");
            let pi = bootstrap(project_root)?;
            let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            elivagar::profile::run(
                &pbf_path,
                &paths.data_dir,
                &paths.scratch_dir,
                tool_name,
                project_root,
            )
        }
        _ => {
            project::require(project, Project::Pbfhogg, "profile")?;

            let pi = bootstrap(project_root)?;
            let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;
            let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
            let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;
            let file_mb = file_size_mb(&pbf_path);

            // Try to get raw PBF path (optional).
            let pbf_raw_path = dev_config
                .datasets
                .get(&dataset)
                .and_then(|ds| ds.pbf_raw.as_ref())
                .map(|raw_file| paths.data_dir.join(raw_file))
                .filter(|p| p.exists());

            pbfhogg::profile::run(
                &pbf_path,
                pbf_raw_path.as_deref(),
                &osc_path,
                &dataset,
                file_mb,
                &paths.scratch_dir,
                project_root,
            )
        }
    }
}

fn cmd_download(
    project: Project,
    project_root: &Path,
    region: String,
    osc_url: Option<String>,
) -> Result<(), DevError> {
    project::require(project, Project::Pbfhogg, "download")?;

    let pi = bootstrap(project_root)?;
    let (dev_config, paths) = bootstrap_config(project_root, &pi.target_dir)?;

    pbfhogg::download::run(
        &region,
        osc_url.as_deref(),
        &paths.data_dir,
        project_root,
    )
}
