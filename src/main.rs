mod build;
mod cli;
mod config;
mod context;
mod db;
mod env;
mod error;
mod git;
mod harness;
mod hotpath_fmt;
mod lockfile;
mod oom;
mod output;
mod pbfhogg;
mod pmtiles;
mod elivagar;
mod nidhogg;
mod preflight;
mod profiler;
mod project;
mod resolve;
mod tools;
mod worktree;

use std::path::Path;
use std::process;

use clap::Parser;

use cli::{BenchCommand, Cli, Command, VerifyCommand};
use context::{acquire_cmd_lock, bootstrap, bootstrap_config, with_worktree};
use error::DevError;
use project::Project;
use resolve::results_db_path;

fn main() {
    let cli = Cli::parse();

    let result = run(cli);
    if let Err(e) = result {
        output::error(&e.to_string());
        process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), DevError> {
    // `lock` works without a project root (global lock file).
    if matches!(cli.command, Command::Lock) {
        return cmd_lock();
    }

    let (project, dev_config, project_root) = project::detect()?;

    match cli.command {
        Command::Lock => unreachable!(),
        Command::Check { args } => cmd_check(project, &project_root, &args),
        Command::Env => cmd_env(&dev_config, project, &project_root),
        Command::Run { features, args } => {
            let _lock = acquire_cmd_lock(project, &project_root, "run")?;
            cmd_run(&dev_config, project, &project_root, &features, &args)
        }
        Command::Results {
            query,
            commit,
            compare,
            compare_last,
            command,
            variant,
            limit,
            top,
        } => cmd_results(&project_root, query, commit, compare, compare_last, command, variant, limit, top),
        Command::Clean => {
            let _lock = acquire_cmd_lock(project, &project_root, "clean")?;
            cmd_clean(&dev_config, project, &project_root)
        }
        Command::Bench { verbose, commit, features, bench } => {
            output::set_quiet(!verbose);
            with_worktree(&project_root, commit.as_deref(), |build_root| {
                cmd_bench(&dev_config, project, &project_root, build_root, bench, &features)
            })
        }
        Command::Verify { verbose, commit, verify } => {
            output::set_quiet(!verbose);
            with_worktree(&project_root, commit.as_deref(), |build_root| {
                cmd_verify(&dev_config, project, &project_root, build_root, verify)
            })
        }
        Command::Hotpath {
            variant,
            verbose,
            commit,
            features,
            dataset,
            pbf,
            osc,
            alloc,
            no_ocean,
            runs,
            tiles,
            nodes,
            no_mem_check,
        } => {
            output::set_quiet(!verbose);
            if features.iter().any(|f| f == "linux-io-uring") {
                preflight::run_preflight(&preflight::uring_checks())?;
            }
            with_worktree(&project_root, commit.as_deref(), |build_root| {
                cmd_hotpath(&dev_config, project, &project_root, build_root, &dataset, pbf.as_deref(), osc.as_deref(), alloc, no_ocean, runs, &features, variant.as_deref(), tiles, nodes, no_mem_check)
            })
        }
        Command::Profile { verbose, commit, features, dataset, pbf, osc, tool, no_ocean, no_mem_check } => {
            output::set_quiet(!verbose);
            if features.iter().any(|f| f == "linux-io-uring") {
                preflight::run_preflight(&preflight::uring_checks())?;
            }
            with_worktree(&project_root, commit.as_deref(), |build_root| {
                cmd_profile(&dev_config, project, &project_root, build_root, &dataset, pbf.as_deref(), osc.as_deref(), tool.as_deref(), no_ocean, &features, no_mem_check)
            })
        }
        Command::Download { region, osc_url } => {
            let _lock = acquire_cmd_lock(project, &project_root, "download")?;
            pbfhogg::cmd::download(&dev_config, project, &project_root, &region, osc_url.as_deref())
        }
        Command::CompareTiles { file_a, file_b, sample } => {
            elivagar::cmd::compare_tiles(project, &project_root, &file_a, &file_b, sample)
        }
        Command::DownloadOcean => {
            let _lock = acquire_cmd_lock(project, &project_root, "download-ocean")?;
            elivagar::cmd::download_ocean(&dev_config, project, &project_root)
        }
        Command::PmtilesStats { files } => cmd_pmtiles_stats(&files),
        Command::Serve { data_dir, dataset, tiles } => {
            let _lock = acquire_cmd_lock(project, &project_root, "serve")?;
            nidhogg::cmd::serve(&dev_config, project, &project_root, data_dir.as_deref(), &dataset, tiles.as_deref())
        }
        Command::Stop => nidhogg::cmd::stop(project, &project_root),
        Command::Status => nidhogg::cmd::status(&dev_config, project, &project_root),
        Command::Ingest { pbf, dataset } => {
            let _lock = acquire_cmd_lock(project, &project_root, "ingest")?;
            nidhogg::cmd::ingest(&dev_config, project, &project_root, pbf.as_deref(), &dataset)
        }
        Command::Update { args } => {
            let _lock = acquire_cmd_lock(project, &project_root, "update")?;
            nidhogg::cmd::update(project, &project_root, &args)
        }
        Command::Query { json } => nidhogg::cmd::query(&dev_config, project, &project_root, json.as_deref()),
        Command::Geocode { term } => nidhogg::cmd::geocode(&dev_config, project, &project_root, &term),
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
        let stdout = String::from_utf8_lossy(&captured.stdout);
        let stderr = String::from_utf8_lossy(&captured.stderr);
        if !stdout.is_empty() {
            output::error(&stdout);
        }
        if !stderr.is_empty() {
            output::error(&stderr);
        }
        return Err(DevError::Build("tests failed".into()));
    }

    Ok(())
}

fn cmd_env(dev_config: &config::DevConfig, project: Project, project_root: &Path) -> Result<(), DevError> {
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let info = env::collect(&paths, project, project_root);
    env::print(&info);
    Ok(())
}

fn cmd_run(dev_config: &config::DevConfig, project: Project, project_root: &Path, features: &[String], args: &[String]) -> Result<(), DevError> {
    // Run uring preflight checks if io_uring feature is requested.
    if features.iter().any(|f| f == "linux-io-uring") {
        preflight::run_preflight(&preflight::uring_checks())?;
    }

    let package = project.cli_package();
    let feature_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let build_config = if feature_refs.is_empty() {
        build::BuildConfig::release(package)
    } else {
        build::BuildConfig::release_with_features(package, &feature_refs)
    };
    let binary = build::cargo_build(&build_config, project_root)?;

    // Ensure scratch dir exists — binary commands often write output there.
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    std::fs::create_dir_all(&paths.scratch_dir)?;

    match project {
        Project::Elivagar => elivagar::cmd::run_elivagar(&paths, &binary, args),
        _ => {
            let binary_str = binary.display().to_string();
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            output::run_msg(&format!("{binary_str} {}", args.join(" ")));
            let code = output::run_passthrough(&binary_str, &arg_refs)?;
            if code != 0 {
                process::exit(code);
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_results(
    project_root: &Path,
    query: Option<String>,
    commit: Option<String>,
    compare: Option<Vec<String>>,
    compare_last: bool,
    command: Option<String>,
    variant: Option<String>,
    limit: usize,
    top: usize,
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
                if let Some(ref hotpath) = row.hotpath
                    && let Some(report) = hotpath_fmt::format_hotpath_report(hotpath, top)
                {
                    println!("\n{report}");
                }
            }
        }
    } else if let Some(commits) = compare {
        let commit_a = commits.first().map_or("", String::as_str);
        let commit_b = commits.get(1).map_or("", String::as_str);
        let (rows_a, rows_b) = results_db.query_compare(commit_a, commit_b, command.as_deref(), variant.as_deref())?;
        let table = db::format_compare(commit_a, &rows_a, commit_b, &rows_b, top);
        println!("{table}");
    } else if compare_last {
        match results_db.query_compare_last(command.as_deref(), variant.as_deref())? {
            Some((commit_a, rows_a, commit_b, rows_b)) => {
                let table = db::format_compare(&commit_a, &rows_a, &commit_b, &rows_b, top);
                println!("{table}");
            }
            None => {
                output::result_msg("need at least two distinct commits to compare");
            }
        }
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
    let pi = bootstrap(None)?;
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

fn cmd_lock() -> Result<(), DevError> {
    match lockfile::status()? {
        Some(info) => {
            println!(
                "[lock]    held by PID {}: {} {} ({})",
                info.pid, info.project, info.command, info.project_root,
            );
        }
        None => {
            println!("[lock]    no active lock");
        }
    }
    Ok(())
}

fn cmd_pmtiles_stats(files: &[String]) -> Result<(), DevError> {
    for file in files {
        pmtiles::run(file)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bench dispatch
// ---------------------------------------------------------------------------

fn cmd_bench(dev_config: &config::DevConfig, project: Project, project_root: &Path, build_root: Option<&Path>, bench: BenchCommand, features: &[String]) -> Result<(), DevError> {
    match bench {
        // ----- pbfhogg bench variants -----
        BenchCommand::Commands { command, dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench commands")?;
            pbfhogg::cmd::bench_commands(dev_config, project, project_root, build_root, &command, &dataset, pbf.as_deref(), runs, features)
        }
        BenchCommand::Extract { dataset, pbf, runs, bbox, strategies } => {
            project::require(project, Project::Pbfhogg, "bench extract")?;
            pbfhogg::cmd::bench_extract(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, bbox.as_deref(), &strategies, features)
        }
        BenchCommand::Allocator { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench allocator")?;
            pbfhogg::cmd::bench_allocator(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, features)
        }
        BenchCommand::BlobFilter { dataset, pbf_indexed, pbf_raw, runs } => {
            project::require(project, Project::Pbfhogg, "bench blob-filter")?;
            pbfhogg::cmd::bench_blob_filter(dev_config, project, project_root, build_root, &dataset, pbf_indexed.as_deref(), pbf_raw.as_deref(), runs, features)
        }
        BenchCommand::Planetiler { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench planetiler")?;
            pbfhogg::cmd::bench_planetiler(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs)
        }
        BenchCommand::Read { dataset, pbf, runs, modes } => {
            project::require(project, Project::Pbfhogg, "bench read")?;
            pbfhogg::cmd::bench_read(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, &modes, features)
        }
        BenchCommand::Write { dataset, pbf, runs, compression } => {
            project::require(project, Project::Pbfhogg, "bench write")?;
            pbfhogg::cmd::bench_write(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, &compression, features)
        }
        BenchCommand::Merge { dataset, pbf, osc, runs, uring, compression } => {
            project::require(project, Project::Pbfhogg, "bench merge")?;
            pbfhogg::cmd::bench_merge(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), osc.as_deref(), runs, uring, &compression, features)
        }
        BenchCommand::All { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench all")?;
            pbfhogg::cmd::bench_all(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, features)
        }

        // ----- elivagar bench variants -----
        BenchCommand::ElivSelf { dataset, pbf, runs, skip_to, no_ocean, compression_level } => {
            project::require(project, Project::Elivagar, "bench self")?;
            elivagar::cmd::bench_self(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, skip_to.as_deref(), no_ocean, compression_level, features)
        }
        BenchCommand::NodeStore { nodes, runs } => {
            project::require(project, Project::Elivagar, "bench node-store")?;
            elivagar::cmd::bench_node_store(dev_config, project, project_root, build_root, nodes, runs)
        }
        BenchCommand::Pmtiles { tiles, runs } => {
            project::require(project, Project::Elivagar, "bench pmtiles")?;
            elivagar::cmd::bench_pmtiles(dev_config, project, project_root, build_root, tiles, runs)
        }
        BenchCommand::ElivPlanetiler { dataset, pbf, runs } => {
            project::require(project, Project::Elivagar, "bench eliv-planetiler")?;
            elivagar::cmd::bench_planetiler(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs)
        }
        BenchCommand::Tilemaker { dataset, pbf, runs } => {
            project::require(project, Project::Elivagar, "bench tilemaker")?;
            elivagar::cmd::bench_tilemaker(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs)
        }
        BenchCommand::ElivAll { dataset, pbf, runs } => {
            project::require(project, Project::Elivagar, "bench eliv-all")?;
            elivagar::cmd::bench_all(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, features)
        }

        // ----- nidhogg bench variants -----
        BenchCommand::Api { dataset, runs, query } => {
            project::require(project, Project::Nidhogg, "bench api")?;
            nidhogg::cmd::bench_api(dev_config, project, project_root, build_root, &dataset, runs, query.as_deref(), features)
        }
        BenchCommand::NidIngest { dataset, pbf, runs } => {
            project::require(project, Project::Nidhogg, "bench ingest")?;
            nidhogg::cmd::bench_ingest(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, features)
        }
    }
}

// ---------------------------------------------------------------------------
// Verify dispatch
// ---------------------------------------------------------------------------

fn cmd_verify(dev_config: &config::DevConfig, project: Project, project_root: &Path, build_root: Option<&Path>, verify: VerifyCommand) -> Result<(), DevError> {
    match verify {
        // ----- nidhogg verify variants -----
        VerifyCommand::Batch => {
            project::require(project, Project::Nidhogg, "verify batch")?;
            nidhogg::cmd::verify_batch(dev_config, project, project_root)
        }
        VerifyCommand::NidGeocode { queries } => {
            project::require(project, Project::Nidhogg, "verify geocode")?;
            nidhogg::cmd::verify_geocode(dev_config, project, project_root, &queries)
        }
        VerifyCommand::Readonly { dataset } => {
            project::require(project, Project::Nidhogg, "verify readonly")?;
            nidhogg::cmd::verify_readonly(dev_config, project, project_root, &dataset)
        }
        // ----- pbfhogg verify variants -----
        _ => {
            project::require(project, Project::Pbfhogg, "verify")?;
            pbfhogg::cmd::verify(dev_config, project, project_root, build_root, verify)
        }
    }
}

// ---------------------------------------------------------------------------
// Hotpath / Profile dispatch
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_hotpath(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    osc: Option<&str>,
    alloc: bool,
    no_ocean: bool,
    runs: usize,
    features: &[String],
    variant: Option<&str>,
    tiles: usize,
    nodes: usize,
    no_mem_check: bool,
) -> Result<(), DevError> {
    if variant.is_some() && project != Project::Elivagar {
        return Err(DevError::Config(
            "hotpath variants (pmtiles, node-store) are only available for elivagar".into(),
        ));
    }

    let feature = harness::hotpath_feature(alloc);
    let mut all_features: Vec<&str> = vec![feature];
    all_features.extend(features.iter().map(String::as_str));

    match project {
        Project::Elivagar => elivagar::cmd::hotpath(dev_config, project, project_root, build_root, dataset, pbf, runs, &all_features, variant, tiles, nodes, no_mem_check, alloc, no_ocean),
        Project::Nidhogg => nidhogg::cmd::hotpath(dev_config, project, project_root, build_root, dataset, pbf, runs, &all_features, no_mem_check, alloc),
        _ => {
            project::require(project, Project::Pbfhogg, "hotpath")?;
            pbfhogg::cmd::hotpath(dev_config, project, project_root, build_root, dataset, pbf, osc, runs, &all_features, no_mem_check, alloc)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_profile(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    osc: Option<&str>,
    tool: Option<&str>,
    no_ocean: bool,
    features: &[String],
    no_mem_check: bool,
) -> Result<(), DevError> {
    match project {
        Project::Elivagar => elivagar::cmd::profile(dev_config, project, project_root, build_root, dataset, pbf, tool, no_ocean, features, no_mem_check),
        Project::Nidhogg => nidhogg::cmd::profile(dev_config, project, project_root, build_root, dataset, pbf, tool, features, no_mem_check),
        _ => {
            project::require(project, Project::Pbfhogg, "profile")?;
            pbfhogg::cmd::profile(dev_config, project, project_root, build_root, dataset, pbf, osc, features, no_mem_check)
        }
    }
}
