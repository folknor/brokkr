mod build;
mod config;
mod db;
mod env;
mod error;
mod git;
mod harness;
mod hotpath_fmt;
mod lockfile;
mod output;
mod pbfhogg;
mod elivagar;
mod nidhogg;
mod preflight;
mod project;
mod tools;

use std::path::{Path, PathBuf};
use std::process;

use clap::{Parser, Subcommand};

use error::DevError;
use project::Project;

#[derive(Parser)]
#[command(name = "brokkr", about = "Shared development tooling")]
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
        /// UUID prefix to look up specific result(s)
        #[arg(conflicts_with_all = ["commit", "compare"])]
        query: Option<String>,

        /// Show results for a specific commit (prefix match)
        #[arg(long, conflicts_with = "compare")]
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
        /// Print full build/bench/result output
        #[arg(long, short = 'v')]
        verbose: bool,

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
        /// Print full build/bench/result output
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Dataset name from brokkr.toml (default: denmark)
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

        /// Skip ocean shapefile detection (elivagar only)
        #[arg(long)]
        no_ocean: bool,

        /// Number of runs (default: 1 for profiling)
        #[arg(long, default_value = "1")]
        runs: usize,
    },
    /// Run two-pass profiling (timing + allocation) for a dataset
    Profile {
        /// Print full build/bench/result output
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Dataset name from brokkr.toml (default: denmark)
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

        /// Skip ocean shapefile detection (elivagar only)
        #[arg(long)]
        no_ocean: bool,
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
    /// Start the nidhogg server (nidhogg only)
    Serve {
        /// Data directory (ingested disk format)
        #[arg(long)]
        data_dir: Option<String>,

        /// Dataset name from brokkr.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Path to PMTiles file for tile serving
        #[arg(long)]
        tiles: Option<String>,
    },
    /// Stop the nidhogg server (nidhogg only)
    Stop,
    /// Check nidhogg server status (nidhogg only)
    Status,
    /// Ingest a PBF into nidhogg disk format (nidhogg only)
    Ingest {
        /// Explicit PBF file path
        #[arg(long)]
        pbf: Option<String>,

        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
    },
    /// Run nidhogg-update for diff application (nidhogg only)
    Update {
        /// Arguments passed to nidhogg-update
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Send a test query to the nidhogg server (nidhogg only)
    Query {
        /// JSON query body (default: Copenhagen highways)
        json: Option<String>,
    },
    /// Test geocoding on the nidhogg server (nidhogg only)
    Geocode {
        /// Search term (default: Kobenhavn)
        #[arg(default_value = "København")]
        term: String,
    },
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
    Tilemaker,
    /// Elivagar: full benchmark suite
    ElivAll {
        #[arg(long, default_value = "denmark")]
        dataset: String,
        #[arg(long)]
        pbf: Option<String>,
        #[arg(long, default_value = "3")]
        runs: usize,
    },

    // ----- Nidhogg bench variants -----

    /// Nidhogg: API query benchmark
    Api {
        /// Dataset the server is loaded with (for metadata recording)
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// Run count
        #[arg(long, default_value = "10")]
        runs: usize,
        /// Only run this specific query (cph_highways, cph_large, cph_small_nofilter, cph_buildings)
        #[arg(long)]
        query: Option<String>,
    },
    /// Nidhogg: ingest benchmark
    NidIngest {
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

    // ----- Nidhogg verify variants -----

    /// Nidhogg: batch query verification
    Batch,
    /// Nidhogg: geocode verification
    NidGeocode {
        /// Search terms to test
        #[arg(trailing_var_arg = true)]
        queries: Vec<String>,
    },
    /// Nidhogg: read-only filesystem verification
    Readonly {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
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
    let (project, dev_config, project_root) = project::detect()?;

    match cli.command {
        Command::Check { args } => cmd_check(project, &project_root, &args),
        Command::Env => cmd_env(&dev_config, project, &project_root),
        Command::Run { args } => cmd_run(&dev_config, project, &project_root, &args),
        Command::Results {
            query,
            commit,
            compare,
            command,
            variant,
            limit,
        } => cmd_results(&project_root, query, commit, compare, command, variant, limit),
        Command::Clean => cmd_clean(&dev_config, project, &project_root),
        Command::Bench { verbose, bench } => {
            output::set_quiet(!verbose);
            cmd_bench(&dev_config, project, &project_root, bench)
        }
        Command::Verify { verify } => cmd_verify(&dev_config, project, &project_root, verify),
        Command::Hotpath {
            verbose,
            dataset,
            pbf,
            osc,
            alloc,
            no_ocean,
            runs,
        } => {
            output::set_quiet(!verbose);
            cmd_hotpath(&dev_config, project, &project_root, &dataset, pbf.as_deref(), osc.as_deref(), alloc, no_ocean, runs)
        }
        Command::Profile { verbose, dataset, pbf, osc, tool, no_ocean } => {
            output::set_quiet(!verbose);
            cmd_profile(&dev_config, project, &project_root, &dataset, pbf.as_deref(), osc.as_deref(), tool.as_deref(), no_ocean)
        }
        Command::Download { region, osc_url } => {
            cmd_download(&dev_config, project, &project_root, &region, osc_url.as_deref())
        }
        Command::CompareTiles { file_a, file_b, sample } => {
            cmd_compare_tiles(project, &project_root, &file_a, &file_b, sample)
        }
        Command::DownloadOcean => cmd_download_ocean(&dev_config, project, &project_root),
        Command::Serve { data_dir, dataset, tiles } => {
            cmd_serve(&dev_config, project, &project_root, data_dir.as_deref(), &dataset, tiles.as_deref())
        }
        Command::Stop => cmd_stop(project, &project_root),
        Command::Status => cmd_status(&dev_config, project, &project_root),
        Command::Ingest { pbf, dataset } => {
            cmd_ingest(&dev_config, project, &project_root, pbf.as_deref(), &dataset)
        }
        Command::Update { args } => cmd_update(project, &project_root, &args),
        Command::Query { json } => cmd_query(&dev_config, project, &project_root, json.as_deref()),
        Command::Geocode { term } => cmd_geocode(&dev_config, project, &project_root, &term),
    }
}

// ---------------------------------------------------------------------------
// Bootstrap helpers
// ---------------------------------------------------------------------------

/// Resolve project info (target_dir) using cargo metadata.
fn bootstrap() -> Result<build::ProjectInfo, DevError> {
    build::project_info()
}

/// Resolve paths for the current host from an already-loaded config.
fn bootstrap_config(
    dev_config: &config::DevConfig,
    project_root: &Path,
    target_dir: &Path,
) -> Result<config::ResolvedPaths, DevError> {
    let hostname = config::hostname()?;
    let paths = config::resolve_paths(dev_config, &hostname, project_root, target_dir);
    Ok(paths)
}

// ---------------------------------------------------------------------------
// BenchContext — shared bootstrap for benchmark command handlers
// ---------------------------------------------------------------------------

struct BenchContext {
    paths: config::ResolvedPaths,
    harness: harness::BenchHarness,
    binary: PathBuf,
}

impl BenchContext {
    fn new(
        dev_config: &config::DevConfig,
        project: Project,
        project_root: &Path,
        package: Option<&str>,
        features: &[&str],
    ) -> Result<Self, DevError> {
        let pi = bootstrap()?;
        let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
        let build_config = if features.is_empty() {
            build::BuildConfig::release(package)
        } else {
            build::BuildConfig::release_with_features(package, features)
        };
        let binary = build::cargo_build(&build_config, project_root)?;
        let harness = harness::BenchHarness::new(&paths, project_root, project)?;
        Ok(Self { paths, harness, binary })
    }
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
    if !extra_args.is_empty() {
        args.push("--");
        let extra_refs: Vec<&str> = extra_args.iter().map(String::as_str).collect();
        args.extend_from_slice(&extra_refs);
    }

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

fn cmd_env(dev_config: &config::DevConfig, project: Project, project_root: &Path) -> Result<(), DevError> {
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let info = env::collect(&paths, project, project_root);
    env::print(&info);
    Ok(())
}

fn cmd_run(dev_config: &config::DevConfig, project: Project, project_root: &Path, args: &[String]) -> Result<(), DevError> {
    let package = project.cli_package();
    let binary = build::cargo_build(
        &build::BuildConfig::release(package),
        project_root,
    )?;

    match project {
        Project::Elivagar => cmd_run_elivagar(dev_config, &binary, project_root, args),
        _ => {
            output::run_msg(&format!("{} {}", binary.display(), args.join(" ")));
            let code = output::run_passthrough(&binary, args)?;
            if code != 0 {
                process::exit(code);
            }
            Ok(())
        }
    }
}

fn cmd_run_elivagar(
    dev_config: &config::DevConfig,
    binary: &Path,
    project_root: &Path,
    raw_args: &[String],
) -> Result<(), DevError> {
    // Parse dev-specific flags from raw args.
    let mut no_ocean = false;
    let mut mem_limit: Option<String> = None;
    let mut passthrough: Vec<String> = Vec::new();

    let mut i = 0;
    while i < raw_args.len() {
        match raw_args[i].as_str() {
            "--no-ocean" => no_ocean = true,
            "--mem" => {
                i += 1;
                if i >= raw_args.len() {
                    return Err(DevError::Config("--mem requires a value (e.g. --mem 8G)".into()));
                }
                mem_limit = Some(raw_args[i].clone());
            }
            other => passthrough.push(other.to_owned()),
        }
        i += 1;
    }

    // Load config for data_dir and scratch_dir.
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    // Inject --tmp-dir if not already provided.
    if !passthrough.iter().any(|a| a == "--tmp-dir") {
        passthrough.push("--tmp-dir".into());
        passthrough.push(paths.scratch_dir.display().to_string());
    }

    // Inject ocean shapefiles if not suppressed and not already provided.
    if !no_ocean {
        let (ocean_full, ocean_simplified) =
            elivagar::bench_self::detect_ocean(&paths.data_dir);

        if !passthrough.iter().any(|a| a == "--ocean")
            && let Some(ref shp) = ocean_full
        {
            passthrough.push("--ocean".into());
            passthrough.push(shp.display().to_string());
        }
        if !passthrough.iter().any(|a| a == "--ocean-simplified")
            && let Some(ref shp) = ocean_simplified
        {
            passthrough.push("--ocean-simplified".into());
            passthrough.push(shp.display().to_string());
        }
    }

    let env = [("HOTPATH_METRICS_SERVER_OFF", "true")];

    // Execute with optional systemd-run memory-limit wrapping.
    let code = if let Some(ref mem) = mem_limit {
        let systemd_run = PathBuf::from("systemd-run");
        let mut wrapped = vec![
            "--scope".into(),
            "-p".into(),
            format!("MemoryMax={mem}"),
            binary.display().to_string(),
        ];
        wrapped.extend(passthrough.iter().cloned());

        output::run_msg(&format!(
            "systemd-run --scope -p MemoryMax={mem} {} {}",
            binary.display(),
            passthrough.join(" "),
        ));

        output::run_passthrough_with_env(&systemd_run, &wrapped, &env)?
    } else {
        output::run_msg(&format!("{} {}", binary.display(), passthrough.join(" ")));
        output::run_passthrough_with_env(binary, &passthrough, &env)?
    };

    if code != 0 {
        process::exit(code);
    }
    Ok(())
}

fn cmd_results(
    project_root: &Path,
    query: Option<String>,
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

    if let Some(uuid_prefix) = query {
        let rows = results_db.query_by_uuid(&uuid_prefix)?;
        if rows.is_empty() {
            output::result_msg("no matching results");
        } else {
            let table = db::format_table(&rows);
            println!("{table}");
            // Show detail fields and hotpath report for UUID lookups.
            for row in &rows {
                let details = db::format_details(row);
                if !details.is_empty() {
                    println!("\n{details}");
                }
                if let Some(ref extra) = row.extra
                    && let Some(report) = hotpath_fmt::format_hotpath_report(extra)
                {
                    println!("\n{report}");
                }
            }
        }
    } else if let Some(commits) = compare {
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

fn cmd_clean(dev_config: &config::DevConfig, project: Project, project_root: &Path) -> Result<(), DevError> {
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

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
        } else if project == Project::Nidhogg {
            // Clean nidhogg scratch temp files
            let ingest_tmp = project_root.join(".ingest_tmp");
            if ingest_tmp.exists() {
                std::fs::remove_dir_all(&ingest_tmp)?;
                output::run_msg("cleaned .ingest_tmp");
            }
            let tilegen_tmp = project_root.join(".tilegen_tmp");
            if tilegen_tmp.exists() {
                std::fs::remove_dir_all(&tilegen_tmp)?;
                output::run_msg("cleaned .tilegen_tmp");
            }
        } else {
            let mut removed = 0u32;
            if let Ok(entries) = std::fs::read_dir(&paths.scratch_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("pbf") {
                        std::fs::remove_file(&path).ok();
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
    pbf: Option<&str>,
    dataset: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let (path, hash, origin) = match pbf {
        Some(p) => (PathBuf::from(p), None, None),
        None => {
            let ds = paths.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            let pbf_file = ds.pbf.as_ref().ok_or_else(|| {
                DevError::Config(format!("dataset '{dataset}' has no pbf configured"))
            })?;
            (
                paths.data_dir.join(pbf_file),
                ds.sha256_pbf.as_deref(),
                ds.origin.as_deref(),
            )
        }
    };

    if !path.exists() {
        return Err(DevError::Config(format!(
            "PBF file not found: {}",
            path.display()
        )));
    }

    if let Some(expected) = hash {
        preflight::verify_file_hash(&path, expected, project_root, origin)?;
    }

    Ok(path)
}

/// Resolve the OSC path from --osc or --dataset.
fn resolve_osc_path(
    osc: Option<&str>,
    dataset: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let (path, hash, origin) = match osc {
        Some(p) => (PathBuf::from(p), None, None),
        None => {
            let ds = paths.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            let osc_file = ds.osc.as_ref().ok_or_else(|| {
                DevError::Config(format!(
                    "dataset '{dataset}' has no osc file configured"
                ))
            })?;
            (
                paths.data_dir.join(osc_file),
                ds.sha256_osc.as_deref(),
                ds.origin.as_deref(),
            )
        }
    };

    if !path.exists() {
        return Err(DevError::Config(format!(
            "OSC file not found: {}",
            path.display()
        )));
    }

    if let Some(expected) = hash {
        preflight::verify_file_hash(&path, expected, project_root, origin)?;
    }

    Ok(path)
}

/// Resolve the bbox from --bbox or dataset config.
fn resolve_bbox(
    bbox: Option<&str>,
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> Result<String, DevError> {
    if let Some(b) = bbox {
        return Ok(b.to_owned());
    }

    let ds = paths.datasets.get(dataset).ok_or_else(|| {
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
    pbf_raw: Option<&str>,
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, DevError> {
    let path = match pbf_raw {
        Some(p) => PathBuf::from(p),
        None => {
            let ds = paths.datasets.get(dataset).ok_or_else(|| {
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
fn file_size_mb(path: &Path) -> Result<f64, DevError> {
    let meta = std::fs::metadata(path)?;
    Ok(meta.len() as f64 / 1_000_000.0)
}

/// Path to the results database for the current project.
fn results_db_path(project_root: &Path) -> PathBuf {
    project_root.join(".brokkr").join("results.db")
}

// ---------------------------------------------------------------------------
// Bench commands (pbfhogg-specific)
// ---------------------------------------------------------------------------

fn cmd_bench(dev_config: &config::DevConfig, project: Project, project_root: &Path, bench: BenchCommand) -> Result<(), DevError> {
    match bench {
        // ----- pbfhogg bench variants -----
        BenchCommand::Commands { command, dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench commands")?;
            cmd_bench_commands(dev_config, project, project_root, &command, &dataset, pbf.as_deref(), runs)
        }
        BenchCommand::Extract { dataset, pbf, runs, bbox, strategies } => {
            project::require(project, Project::Pbfhogg, "bench extract")?;
            cmd_bench_extract(dev_config, project, project_root, &dataset, pbf.as_deref(), runs, bbox.as_deref(), &strategies)
        }
        BenchCommand::Allocator { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench allocator")?;
            cmd_bench_allocator(dev_config, project, project_root, &dataset, pbf.as_deref(), runs)
        }
        BenchCommand::BlobFilter { dataset, pbf_indexed, pbf_raw, runs } => {
            project::require(project, Project::Pbfhogg, "bench blob-filter")?;
            cmd_bench_blob_filter(dev_config, project, project_root, &dataset, pbf_indexed.as_deref(), pbf_raw.as_deref(), runs)
        }
        BenchCommand::Planetiler { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench planetiler")?;
            cmd_bench_planetiler(dev_config, project, project_root, &dataset, pbf.as_deref(), runs)
        }
        BenchCommand::Read { dataset, pbf, runs, modes } => {
            project::require(project, Project::Pbfhogg, "bench read")?;
            cmd_bench_read(dev_config, project, project_root, &dataset, pbf.as_deref(), runs, &modes)
        }
        BenchCommand::Write { dataset, pbf, runs, compression } => {
            project::require(project, Project::Pbfhogg, "bench write")?;
            cmd_bench_write(dev_config, project, project_root, &dataset, pbf.as_deref(), runs, &compression)
        }
        BenchCommand::Merge { dataset, pbf, osc, runs, uring, compression } => {
            project::require(project, Project::Pbfhogg, "bench merge")?;
            cmd_bench_merge(dev_config, project, project_root, &dataset, pbf.as_deref(), osc.as_deref(), runs, uring, &compression)
        }
        BenchCommand::All { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench all")?;
            cmd_bench_all(dev_config, project, project_root, &dataset, pbf.as_deref(), runs)
        }

        // ----- elivagar bench variants -----
        BenchCommand::ElivSelf { dataset, pbf, runs, skip_to, no_ocean, compression_level } => {
            project::require(project, Project::Elivagar, "bench self")?;
            cmd_bench_eliv_self(dev_config, project, project_root, &dataset, pbf.as_deref(), runs, skip_to.as_deref(), no_ocean, compression_level)
        }
        BenchCommand::NodeStore { nodes, runs } => {
            project::require(project, Project::Elivagar, "bench node-store")?;
            let pi = bootstrap()?;
            let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
            let harness = harness::BenchHarness::new(&paths, project_root, project)?;
            elivagar::bench_node_store::run(&harness, project_root, nodes, runs)
        }
        BenchCommand::Pmtiles { tiles, runs } => {
            project::require(project, Project::Elivagar, "bench pmtiles")?;
            let pi = bootstrap()?;
            let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
            let harness = harness::BenchHarness::new(&paths, project_root, project)?;
            elivagar::bench_pmtiles::run(&harness, project_root, tiles, runs)
        }
        BenchCommand::ElivPlanetiler { dataset, pbf, runs } => {
            project::require(project, Project::Elivagar, "bench eliv-planetiler")?;
            cmd_bench_eliv_planetiler(dev_config, project, project_root, &dataset, pbf.as_deref(), runs)
        }
        BenchCommand::Tilemaker => {
            project::require(project, Project::Elivagar, "bench tilemaker")?;
            elivagar::bench_tilemaker::run()
        }
        BenchCommand::ElivAll { dataset, pbf, runs } => {
            project::require(project, Project::Elivagar, "bench eliv-all")?;
            cmd_bench_eliv_all(dev_config, project, project_root, &dataset, pbf.as_deref(), runs)
        }

        // ----- nidhogg bench variants -----
        BenchCommand::Api { dataset, runs, query } => {
            project::require(project, Project::Nidhogg, "bench api")?;
            cmd_bench_api(dev_config, project, project_root, &dataset, runs, query.as_deref())
        }
        BenchCommand::NidIngest { dataset, pbf, runs } => {
            project::require(project, Project::Nidhogg, "bench ingest")?;
            cmd_bench_nid_ingest(dev_config, project, project_root, &dataset, pbf.as_deref(), runs)
        }
    }
}

fn cmd_bench_commands(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    command: &str,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let ctx = BenchContext::new(dev_config, project, project_root, Some("pbfhogg-cli"), &[])?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &ctx.paths, project_root)?;
    let commands = pbfhogg::bench_commands::parse_command(command)?;
    let file_mb = file_size_mb(&pbf_path)?;
    let osc_path = resolve_osc_path(None, dataset, &ctx.paths, project_root).ok();
    pbfhogg::bench_commands::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        osc_path.as_deref(),
        Some(&ctx.paths.scratch_dir),
        file_mb,
        runs,
        &commands,
        project_root,
    )
}

fn cmd_bench_extract(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    bbox: Option<&str>,
    strategies_str: &str,
) -> Result<(), DevError> {
    let ctx = BenchContext::new(dev_config, project, project_root, Some("pbfhogg-cli"), &[])?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &ctx.paths, project_root)?;
    let bbox = resolve_bbox(bbox, dataset, &ctx.paths)?;
    let strategies = pbfhogg::bench_extract::parse_strategies(strategies_str)?;
    let file_mb = file_size_mb(&pbf_path)?;
    pbfhogg::bench_extract::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, runs, &bbox, &strategies, project_root)
}

fn cmd_bench_allocator(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &paths, project_root)?;
    let file_mb = file_size_mb(&pbf_path)?;
    let harness = harness::BenchHarness::new(&paths, project_root, project)?;
    pbfhogg::bench_allocator::run(&harness, &pbf_path, file_mb, runs, project_root)
}

fn cmd_bench_blob_filter(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf_indexed: Option<&str>,
    pbf_raw: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let ctx = BenchContext::new(dev_config, project, project_root, Some("pbfhogg-cli"), &[])?;
    let indexed_path = resolve_pbf_path(pbf_indexed, dataset, &ctx.paths, project_root)?;
    let raw_path = resolve_raw_pbf_path(pbf_raw, dataset, &ctx.paths)?;
    let file_mb = file_size_mb(&indexed_path)?;
    pbfhogg::bench_blob_filter::run(&ctx.harness, &ctx.binary, &indexed_path, &raw_path, file_mb, runs, project_root)
}

fn cmd_bench_planetiler(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &paths, project_root)?;
    let file_mb = file_size_mb(&pbf_path)?;
    let harness = harness::BenchHarness::new(&paths, project_root, project)?;
    pbfhogg::bench_planetiler::run(&harness, &pbf_path, file_mb, runs, &paths.data_dir, project_root)
}

fn cmd_bench_read(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    modes_str: &str,
) -> Result<(), DevError> {
    let ctx = BenchContext::new(dev_config, project, project_root, Some("pbfhogg-cli"), &[])?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &ctx.paths, project_root)?;
    let modes = pbfhogg::bench_read::parse_modes(modes_str)?;
    let file_mb = file_size_mb(&pbf_path)?;
    pbfhogg::bench_read::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, runs, &modes, project_root)
}

fn cmd_bench_write(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    compression_str: &str,
) -> Result<(), DevError> {
    let ctx = BenchContext::new(dev_config, project, project_root, Some("pbfhogg-cli"), &[])?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &ctx.paths, project_root)?;
    let compressions = pbfhogg::parse_compressions(compression_str, true)?;
    let file_mb = file_size_mb(&pbf_path)?;
    pbfhogg::bench_write::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, runs, &compressions, project_root)
}

#[allow(clippy::too_many_arguments)]
fn cmd_bench_merge(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    osc: Option<&str>,
    runs: usize,
    uring: bool,
    compression_str: &str,
) -> Result<(), DevError> {
    if uring {
        preflight::run_preflight(&preflight::uring_checks())?;
    }

    let ctx = BenchContext::new(dev_config, project, project_root, Some("pbfhogg-cli"), &[])?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &ctx.paths, project_root)?;
    let osc_path = resolve_osc_path(osc, dataset, &ctx.paths, project_root)?;
    let compressions = pbfhogg::parse_compressions(compression_str, false)?;
    let file_mb = file_size_mb(&pbf_path)?;
    pbfhogg::bench_merge::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        &osc_path,
        file_mb,
        runs,
        &compressions,
        uring,
        &ctx.paths.scratch_dir,
        project_root,
    )
}

fn cmd_bench_all(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &paths, project_root)?;
    let file_mb = file_size_mb(&pbf_path)?;
    let harness = harness::BenchHarness::new(&paths, project_root, project)?;
    pbfhogg::bench_all::run(&harness, &paths, project_root, &pbf_path, file_mb, runs, dataset)
}

// ---------------------------------------------------------------------------
// Bench commands (elivagar-specific)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_bench_eliv_self(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    skip_to: Option<&str>,
    no_ocean: bool,
    compression_level: Option<u32>,
) -> Result<(), DevError> {
    let ctx = BenchContext::new(dev_config, project, project_root, None, &[])?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &ctx.paths, project_root)?;
    let file_mb = file_size_mb(&pbf_path)?;
    elivagar::bench_self::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        file_mb,
        runs,
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        project_root,
        skip_to,
        no_ocean,
        compression_level,
    )
}

fn cmd_bench_eliv_planetiler(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &paths, project_root)?;
    let file_mb = file_size_mb(&pbf_path)?;
    let harness = harness::BenchHarness::new(&paths, project_root, project)?;
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
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &paths, project_root)?;
    let file_mb = file_size_mb(&pbf_path)?;
    let harness = harness::BenchHarness::new(&paths, project_root, project)?;
    elivagar::bench_all::run(
        &harness,
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
    let pi = bootstrap()?;
    elivagar::compare_tiles::run(&pi.target_dir, project_root, file_a, file_b, sample)
}

fn cmd_download_ocean(dev_config: &config::DevConfig, project: Project, project_root: &Path) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "download-ocean")?;
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    elivagar::download_ocean::run(&paths.data_dir)
}

// ---------------------------------------------------------------------------
// Verify commands (pbfhogg-specific)
// ---------------------------------------------------------------------------

fn cmd_verify(dev_config: &config::DevConfig, project: Project, project_root: &Path, verify: VerifyCommand) -> Result<(), DevError> {
    match verify {
        // ----- nidhogg verify variants -----
        VerifyCommand::Batch => {
            project::require(project, Project::Nidhogg, "verify batch")?;
            cmd_verify_batch(dev_config, project, project_root)
        }
        VerifyCommand::NidGeocode { queries } => {
            project::require(project, Project::Nidhogg, "verify geocode")?;
            cmd_verify_geocode(dev_config, project, project_root, &queries)
        }
        VerifyCommand::Readonly { dataset } => {
            project::require(project, Project::Nidhogg, "verify readonly")?;
            cmd_verify_readonly(dev_config, project, project_root, &dataset)
        }
        // ----- pbfhogg verify variants -----
        _ => {
            project::require(project, Project::Pbfhogg, "verify")?;
            cmd_verify_pbfhogg(dev_config, project, project_root, verify)
        }
    }
}

fn cmd_verify_pbfhogg(dev_config: &config::DevConfig, _project: Project, project_root: &Path, verify: VerifyCommand) -> Result<(), DevError> {
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let harness = pbfhogg::verify::VerifyHarness::new(&paths, project_root, &pi.target_dir)?;

    match verify {
        VerifyCommand::Sort { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            pbfhogg::verify_sort::run(&harness, &pbf_path)
        }
        VerifyCommand::Cat { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            pbfhogg::verify_cat::run(&harness, &pbf_path)
        }
        VerifyCommand::Extract { dataset, pbf, bbox } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            let bbox = resolve_bbox(bbox.as_deref(), &dataset, &paths)?;
            pbfhogg::verify_extract::run(&harness, &pbf_path, &bbox)
        }
        VerifyCommand::TagsFilter { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            pbfhogg::verify_tags_filter::run(&harness, &pbf_path)
        }
        VerifyCommand::GetidRemoveid { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            pbfhogg::verify_getid_removeid::run(&harness, &pbf_path)
        }
        VerifyCommand::AddLocationsToWays { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            pbfhogg::verify_add_locations::run(&harness, &pbf_path)
        }
        VerifyCommand::CheckRefs { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            pbfhogg::verify_check_refs::run(&harness, &pbf_path)
        }
        VerifyCommand::Merge { dataset, pbf, osc } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            let osc_path = resolve_osc_path(osc.as_deref(), &dataset, &paths, project_root)?;
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
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            let osc_path = resolve_osc_path(osc.as_deref(), &dataset, &paths, project_root)?;
            pbfhogg::verify_derive_changes::run(&harness, &pbf_path, &osc_path)
        }
        VerifyCommand::Diff { dataset, pbf, osc } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            let osc_path = resolve_osc_path(osc.as_deref(), &dataset, &paths, project_root)?;
            pbfhogg::verify_diff::run(&harness, &pbf_path, &osc_path)
        }
        VerifyCommand::All { dataset, pbf, osc, bbox } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            let osc_path = resolve_osc_path(osc.as_deref(), &dataset, &paths, project_root).ok();
            let bbox_str = resolve_bbox(bbox.as_deref(), &dataset, &paths).ok();
            pbfhogg::verify_all::run(
                &harness,
                &pbf_path,
                osc_path.as_deref(),
                bbox_str.as_deref(),
                &paths.data_dir,
                project_root,
            )
        }
        // Nidhogg variants are handled above in cmd_verify().
        VerifyCommand::Batch
        | VerifyCommand::NidGeocode { .. }
        | VerifyCommand::Readonly { .. } => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Hotpath / Profile / Download
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_hotpath(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    osc: Option<&str>,
    alloc: bool,
    no_ocean: bool,
    runs: usize,
) -> Result<(), DevError> {
    let feature = if alloc { "hotpath-alloc" } else { "hotpath" };

    match project {
        Project::Elivagar => {
            let ctx = BenchContext::new(dev_config, project, project_root, None, &[feature])?;
            let pbf_path = resolve_pbf_path(pbf, dataset, &ctx.paths, project_root)?;
            let file_mb = file_size_mb(&pbf_path)?;
            elivagar::hotpath::run(
                &ctx.harness,
                &ctx.binary,
                &pbf_path,
                &ctx.paths.data_dir,
                &ctx.paths.scratch_dir,
                file_mb,
                runs,
                alloc,
                no_ocean,
                project_root,
            )
        }
        Project::Nidhogg => {
            let ctx = BenchContext::new(dev_config, project, project_root, Some("nidhogg"), &[feature])?;
            let pbf_path = resolve_pbf_path(pbf, dataset, &ctx.paths, project_root)?;
            let file_mb = file_size_mb(&pbf_path)?;
            nidhogg::hotpath::run(
                &ctx.harness,
                &ctx.binary,
                &pbf_path,
                &ctx.paths.scratch_dir,
                file_mb,
                runs,
                alloc,
                project_root,
            )
        }
        _ => {
            project::require(project, Project::Pbfhogg, "hotpath")?;

            let ctx = BenchContext::new(dev_config, project, project_root, Some("pbfhogg-cli"), &[feature])?;
            let pbf_path = resolve_pbf_path(pbf, dataset, &ctx.paths, project_root)?;
            let osc_path = resolve_osc_path(osc, dataset, &ctx.paths, project_root)?;
            let file_mb = file_size_mb(&pbf_path)?;

            // Try to get raw PBF path (optional).
            let pbf_raw_path = ctx.paths
                .datasets
                .get(dataset)
                .and_then(|ds| ds.pbf_raw.as_ref())
                .map(|raw_file| ctx.paths.data_dir.join(raw_file))
                .filter(|p| p.exists());

            pbfhogg::hotpath::run(
                &ctx.harness,
                &ctx.binary,
                &pbf_path,
                pbf_raw_path.as_deref(),
                &osc_path,
                file_mb,
                runs,
                alloc,
                &ctx.paths.scratch_dir,
                project_root,
            )
        }
    }
}

fn cmd_profile(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    osc: Option<&str>,
    tool: Option<&str>,
    no_ocean: bool,
) -> Result<(), DevError> {
    match project {
        Project::Elivagar => {
            let tool_name = tool.unwrap_or("perf");
            let pi = bootstrap()?;
            let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
            let pbf_path = resolve_pbf_path(pbf, dataset, &paths, project_root)?;
            elivagar::profile::run(
                &pbf_path,
                &paths.data_dir,
                &paths.scratch_dir,
                tool_name,
                no_ocean,
                project_root,
            )
        }
        Project::Nidhogg => {
            let tool_name = tool.unwrap_or("perf");
            let pi = bootstrap()?;
            let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
            let pbf_path = resolve_pbf_path(pbf, dataset, &paths, project_root)?;

            let data_dir = paths
                .datasets
                .get(dataset)
                .and_then(|ds| ds.data_dir.as_ref())
                .map(|d| paths.data_dir.join(d))
                .unwrap_or_else(|| paths.data_dir.clone());

            nidhogg::profile::run(
                &pbf_path,
                &data_dir,
                &paths.scratch_dir,
                tool_name,
                project_root,
            )
        }
        _ => {
            project::require(project, Project::Pbfhogg, "profile")?;

            let pi = bootstrap()?;
            let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
            let pbf_path = resolve_pbf_path(pbf, dataset, &paths, project_root)?;
            let osc_path = resolve_osc_path(osc, dataset, &paths, project_root)?;
            let file_mb = file_size_mb(&pbf_path)?;

            // Try to get raw PBF path (optional).
            let pbf_raw_path = paths
                .datasets
                .get(dataset)
                .and_then(|ds| ds.pbf_raw.as_ref())
                .map(|raw_file| paths.data_dir.join(raw_file))
                .filter(|p| p.exists());

            let harness = harness::BenchHarness::new(&paths, project_root, project)?;
            pbfhogg::profile::run(
                &harness,
                &pbf_path,
                pbf_raw_path.as_deref(),
                &osc_path,
                dataset,
                file_mb,
                &paths.scratch_dir,
                project_root,
            )
        }
    }
}

fn cmd_download(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    region: &str,
    osc_url: Option<&str>,
) -> Result<(), DevError> {
    project::require(project, Project::Pbfhogg, "download")?;

    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    pbfhogg::download::run(
        region,
        osc_url,
        &paths.data_dir,
        project_root,
    )
}

// ---------------------------------------------------------------------------
// Nidhogg commands
// ---------------------------------------------------------------------------

fn resolve_nidhogg_port(dev_config: &config::DevConfig) -> u16 {
    // Check PORT env var first
    if let Ok(port_str) = std::env::var("PORT")
        && let Ok(port) = port_str.parse::<u16>() {
            return port;
        }
    // Try brokkr.toml host config
    if let Ok(hostname) = config::hostname()
        && let Some(host) = dev_config.hosts.get(&hostname)
            && let Some(port) = host.port {
                return port;
            }
    nidhogg::server::DEFAULT_PORT
}

fn cmd_serve(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    data_dir: Option<&str>,
    dataset: &str,
    tiles: Option<&str>,
) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "serve")?;
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let data_dir_str = match data_dir {
        Some(d) => d.to_owned(),
        None => {
            let ds = paths.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            let dir_name = ds.data_dir.as_ref().ok_or_else(|| {
                DevError::Config(format!("dataset '{dataset}' has no data_dir configured"))
            })?;
            paths.data_dir.join(dir_name).display().to_string()
        }
    };

    let port = resolve_nidhogg_port(dev_config);
    let binary = build::cargo_build(&build::BuildConfig::release(Some("nidhogg")), project_root)?;
    nidhogg::server::serve(&binary, &data_dir_str, tiles, port, project_root)
}

fn cmd_stop(project: Project, project_root: &Path) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "stop")?;
    nidhogg::server::stop(project_root)
}

fn cmd_status(dev_config: &config::DevConfig, project: Project, _project_root: &Path) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "status")?;
    let port = resolve_nidhogg_port(dev_config);
    let running = nidhogg::server::status(port)?;
    if running {
        output::run_msg(&format!("server running on port {port}"));
    } else {
        output::run_msg(&format!("server not running on port {port}"));
    }
    Ok(())
}

fn cmd_ingest(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    pbf: Option<&str>,
    dataset: &str,
) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "ingest")?;
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &paths, project_root)?;

    let ds = paths.datasets.get(dataset).ok_or_else(|| {
        DevError::Config(format!("unknown dataset: {dataset}"))
    })?;
    let dir_name = ds.data_dir.as_ref().ok_or_else(|| {
        DevError::Config(format!("dataset '{dataset}' has no data_dir configured"))
    })?;
    let data_dir = paths.data_dir.join(dir_name);

    let binary = build::cargo_build(&build::BuildConfig::release(Some("nidhogg")), project_root)?;
    nidhogg::ingest::run(&binary, &pbf_path, &data_dir, project_root)
}

fn cmd_update(project: Project, project_root: &Path, args: &[String]) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "update")?;
    let mut config = build::BuildConfig::release(Some("nidhogg"));
    config.bin = Some("nidhogg-update".into());
    let binary = build::cargo_build(&config, project_root)?;
    nidhogg::update::run(&binary, args, project_root)
}

fn cmd_query(dev_config: &config::DevConfig, project: Project, _project_root: &Path, json: Option<&str>) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "query")?;
    let port = resolve_nidhogg_port(dev_config);
    nidhogg::query::run(port, json)
}

fn cmd_geocode(dev_config: &config::DevConfig, project: Project, _project_root: &Path, term: &str) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "geocode")?;
    let port = resolve_nidhogg_port(dev_config);
    nidhogg::geocode::run(port, term)
}

fn cmd_bench_api(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    runs: usize,
    query: Option<&str>,
) -> Result<(), DevError> {
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let port = resolve_nidhogg_port(dev_config);

    // Resolve dataset PBF for metadata recording.
    let pbf_path = resolve_pbf_path(None, dataset, &paths, project_root).ok();
    let input_file = pbf_path.as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    let input_mb = pbf_path.as_ref().map(|p| file_size_mb(p)).transpose()?;

    let harness = harness::BenchHarness::new(&paths, project_root, project)?;
    nidhogg::bench_api::run(&harness, port, runs, query, input_file, input_mb)
}

fn cmd_bench_nid_ingest(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let ctx = BenchContext::new(dev_config, project, project_root, Some("nidhogg"), &[])?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &ctx.paths, project_root)?;
    let file_mb = file_size_mb(&pbf_path)?;
    nidhogg::bench_ingest::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, runs, &ctx.paths.scratch_dir, project_root)
}

fn cmd_verify_batch(dev_config: &config::DevConfig, _project: Project, _project_root: &Path) -> Result<(), DevError> {
    let port = resolve_nidhogg_port(dev_config);
    nidhogg::verify_batch::run(port)
}

fn cmd_verify_geocode(dev_config: &config::DevConfig, _project: Project, _project_root: &Path, queries: &[String]) -> Result<(), DevError> {
    let port = resolve_nidhogg_port(dev_config);
    let default_queries = ["Kobenhavn", "Aarhus", "Odense"];
    let query_refs: Vec<&str> = if queries.is_empty() {
        default_queries.to_vec()
    } else {
        queries.iter().map(String::as_str).collect()
    };
    nidhogg::verify_geocode::run(port, &query_refs)
}

fn cmd_verify_readonly(dev_config: &config::DevConfig, _project: Project, project_root: &Path, dataset: &str) -> Result<(), DevError> {
    let pi = bootstrap()?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let port = resolve_nidhogg_port(dev_config);

    let ds = paths.datasets.get(dataset).ok_or_else(|| {
        DevError::Config(format!("unknown dataset: {dataset}"))
    })?;
    let dir_name = ds.data_dir.as_ref().ok_or_else(|| {
        DevError::Config(format!("dataset '{dataset}' has no data_dir configured"))
    })?;
    let data_dir_str = paths.data_dir.join(dir_name).display().to_string();

    let binary = build::cargo_build(&build::BuildConfig::release(Some("nidhogg")), project_root)?;
    nidhogg::verify_readonly::run(&binary, &data_dir_str, port, project_root)
}
