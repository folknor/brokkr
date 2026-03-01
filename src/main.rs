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
use context::{acquire_cmd_lock, bootstrap, bootstrap_config, with_worktree, BenchContext, HarnessContext};
use error::DevError;
use project::Project;
use resolve::{
    file_size_mb, resolve_bbox, resolve_nidhogg_data_dir, resolve_osc_path, resolve_pbf_path,
    resolve_pbf_with_size, resolve_raw_pbf_path, results_db_path,
};

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
            cmd_download(&dev_config, project, &project_root, &region, osc_url.as_deref())
        }
        Command::CompareTiles { file_a, file_b, sample } => {
            cmd_compare_tiles(project, &project_root, &file_a, &file_b, sample)
        }
        Command::DownloadOcean => {
            let _lock = acquire_cmd_lock(project, &project_root, "download-ocean")?;
            cmd_download_ocean(&dev_config, project, &project_root)
        }
        Command::PmtilesStats { files } => cmd_pmtiles_stats(&files),
        Command::Serve { data_dir, dataset, tiles } => {
            let _lock = acquire_cmd_lock(project, &project_root, "serve")?;
            cmd_serve(&dev_config, project, &project_root, data_dir.as_deref(), &dataset, tiles.as_deref())
        }
        Command::Stop => cmd_stop(project, &project_root),
        Command::Status => cmd_status(&dev_config, project, &project_root),
        Command::Ingest { pbf, dataset } => {
            let _lock = acquire_cmd_lock(project, &project_root, "ingest")?;
            cmd_ingest(&dev_config, project, &project_root, pbf.as_deref(), &dataset)
        }
        Command::Update { args } => {
            let _lock = acquire_cmd_lock(project, &project_root, "update")?;
            cmd_update(project, &project_root, &args)
        }
        Command::Query { json } => cmd_query(&dev_config, project, &project_root, json.as_deref()),
        Command::Geocode { term } => cmd_geocode(&dev_config, project, &project_root, &term),
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
        Project::Elivagar => cmd_run_elivagar(&paths, &binary, args),
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

fn cmd_run_elivagar(
    paths: &config::ResolvedPaths,
    binary: &Path,
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

    // Inject --tmp-dir if not already provided.
    if !passthrough.iter().any(|a| a == "--tmp-dir") {
        passthrough.push("--tmp-dir".into());
        passthrough.push(paths.scratch_dir.display().to_string());
    }

    // Inject ocean shapefiles if not suppressed and not already provided.
    if !no_ocean {
        let (ocean_full, ocean_simplified) =
            elivagar::detect_ocean(&paths.data_dir);

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
    let binary_str = binary.display().to_string();
    let code = if let Some(ref mem) = mem_limit {
        let mem_arg = format!("MemoryMax={mem}");
        let mut wrapped: Vec<&str> = vec!["--scope", "-p", &mem_arg, &binary_str];
        let pt_refs: Vec<&str> = passthrough.iter().map(String::as_str).collect();
        wrapped.extend_from_slice(&pt_refs);

        output::run_msg(&format!(
            "systemd-run --scope -p {mem_arg} {binary_str} {}",
            passthrough.join(" "),
        ));

        output::run_passthrough_with_env("systemd-run", &wrapped, &env)?
    } else {
        let pt_refs: Vec<&str> = passthrough.iter().map(String::as_str).collect();
        output::run_msg(&format!("{binary_str} {}", passthrough.join(" ")));
        output::run_passthrough_with_env(&binary_str, &pt_refs, &env)?
    };

    if code != 0 {
        process::exit(code);
    }
    Ok(())
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

// ---------------------------------------------------------------------------
// Bench commands (pbfhogg-specific)
// ---------------------------------------------------------------------------

fn cmd_bench(dev_config: &config::DevConfig, project: Project, project_root: &Path, build_root: Option<&Path>, bench: BenchCommand, features: &[String]) -> Result<(), DevError> {
    match bench {
        // ----- pbfhogg bench variants -----
        BenchCommand::Commands { command, dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench commands")?;
            cmd_bench_commands(dev_config, project, project_root, build_root, &command, &dataset, pbf.as_deref(), runs, features)
        }
        BenchCommand::Extract { dataset, pbf, runs, bbox, strategies } => {
            project::require(project, Project::Pbfhogg, "bench extract")?;
            cmd_bench_extract(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, bbox.as_deref(), &strategies, features)
        }
        BenchCommand::Allocator { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench allocator")?;
            cmd_bench_allocator(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, features)
        }
        BenchCommand::BlobFilter { dataset, pbf_indexed, pbf_raw, runs } => {
            project::require(project, Project::Pbfhogg, "bench blob-filter")?;
            cmd_bench_blob_filter(dev_config, project, project_root, build_root, &dataset, pbf_indexed.as_deref(), pbf_raw.as_deref(), runs, features)
        }
        BenchCommand::Planetiler { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench planetiler")?;
            cmd_bench_planetiler(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs)
        }
        BenchCommand::Read { dataset, pbf, runs, modes } => {
            project::require(project, Project::Pbfhogg, "bench read")?;
            cmd_bench_read(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, &modes, features)
        }
        BenchCommand::Write { dataset, pbf, runs, compression } => {
            project::require(project, Project::Pbfhogg, "bench write")?;
            cmd_bench_write(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, &compression, features)
        }
        BenchCommand::Merge { dataset, pbf, osc, runs, uring, compression } => {
            project::require(project, Project::Pbfhogg, "bench merge")?;
            cmd_bench_merge(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), osc.as_deref(), runs, uring, &compression, features)
        }
        BenchCommand::All { dataset, pbf, runs } => {
            project::require(project, Project::Pbfhogg, "bench all")?;
            cmd_bench_all(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, features)
        }

        // ----- elivagar bench variants -----
        BenchCommand::ElivSelf { dataset, pbf, runs, skip_to, no_ocean, compression_level } => {
            project::require(project, Project::Elivagar, "bench self")?;
            cmd_bench_eliv_self(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, skip_to.as_deref(), no_ocean, compression_level, features)
        }
        BenchCommand::NodeStore { nodes, runs } => {
            project::require(project, Project::Elivagar, "bench node-store")?;
            let pi = bootstrap(build_root)?;
            let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
            let db_root = build_root.map(|_| project_root);
            let effective = build_root.unwrap_or(project_root);
            let harness = harness::BenchHarness::new(&paths, effective, db_root, project, "bench node-store")?;
            elivagar::bench_node_store::run(&harness, effective, nodes, runs)
        }
        BenchCommand::Pmtiles { tiles, runs } => {
            project::require(project, Project::Elivagar, "bench pmtiles")?;
            let pi = bootstrap(build_root)?;
            let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
            let db_root = build_root.map(|_| project_root);
            let effective = build_root.unwrap_or(project_root);
            let harness = harness::BenchHarness::new(&paths, effective, db_root, project, "bench pmtiles")?;
            elivagar::bench_pmtiles::run(&harness, effective, tiles, runs)
        }
        BenchCommand::ElivPlanetiler { dataset, pbf, runs } => {
            project::require(project, Project::Elivagar, "bench eliv-planetiler")?;
            cmd_bench_eliv_planetiler(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs)
        }
        BenchCommand::Tilemaker { dataset, pbf, runs } => {
            project::require(project, Project::Elivagar, "bench tilemaker")?;
            cmd_bench_tilemaker(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs)
        }
        BenchCommand::ElivAll { dataset, pbf, runs } => {
            project::require(project, Project::Elivagar, "bench eliv-all")?;
            cmd_bench_eliv_all(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, features)
        }

        // ----- nidhogg bench variants -----
        BenchCommand::Api { dataset, runs, query } => {
            project::require(project, Project::Nidhogg, "bench api")?;
            cmd_bench_api(dev_config, project, project_root, build_root, &dataset, runs, query.as_deref(), features)
        }
        BenchCommand::NidIngest { dataset, pbf, runs } => {
            project::require(project, Project::Nidhogg, "bench ingest")?;
            cmd_bench_nid_ingest(dev_config, project, project_root, build_root, &dataset, pbf.as_deref(), runs, features)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_bench_commands(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    command: &str,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench commands")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let commands = pbfhogg::bench_commands::parse_command(command)?;
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

#[allow(clippy::too_many_arguments)]
fn cmd_bench_extract(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    bbox: Option<&str>,
    strategies_str: &str,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench extract")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let bbox = resolve_bbox(bbox, dataset, &ctx.paths)?;
    let strategies = pbfhogg::bench_extract::parse_strategies(strategies_str)?;
    pbfhogg::bench_extract::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, runs, &bbox, &strategies, project_root)
}

#[allow(clippy::too_many_arguments)]
fn cmd_bench_allocator(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    _features: &[String],
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "bench allocator")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let effective = build_root.unwrap_or(project_root);
    pbfhogg::bench_allocator::run(&ctx.harness, &pbf_path, file_mb, runs, effective)
}

#[allow(clippy::too_many_arguments)]
fn cmd_bench_blob_filter(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf_indexed: Option<&str>,
    pbf_raw: Option<&str>,
    runs: usize,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench blob-filter")?;
    let (indexed_path, file_mb) = resolve_pbf_with_size(pbf_indexed, dataset, &ctx.paths, project_root)?;
    let raw_path = resolve_raw_pbf_path(pbf_raw, dataset, &ctx.paths)?;
    pbfhogg::bench_blob_filter::run(&ctx.harness, &ctx.binary, &indexed_path, &raw_path, file_mb, runs, project_root)
}

fn cmd_bench_planetiler(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "bench planetiler")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    pbfhogg::bench_planetiler::run(&ctx.harness, &pbf_path, file_mb, runs, &ctx.paths.data_dir, project_root)
}

#[allow(clippy::too_many_arguments)]
fn cmd_bench_read(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    modes_str: &str,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench read")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let modes = pbfhogg::bench_read::parse_modes(modes_str)?;
    pbfhogg::bench_read::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, runs, &modes, project_root)
}

#[allow(clippy::too_many_arguments)]
fn cmd_bench_write(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    compression_str: &str,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench write")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let compressions = pbfhogg::parse_compressions(compression_str, true)?;
    pbfhogg::bench_write::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, runs, &compressions, project_root)
}

#[allow(clippy::too_many_arguments)]
fn cmd_bench_merge(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    osc: Option<&str>,
    runs: usize,
    uring: bool,
    compression_str: &str,
    features: &[String],
) -> Result<(), DevError> {
    if uring {
        preflight::run_preflight(&preflight::uring_checks())?;
    }

    let mut all_features: Vec<&str> = features.iter().map(String::as_str).collect();
    if uring {
        all_features.push("linux-io-uring");
    }
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &all_features, true, "bench merge")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let osc_path = resolve_osc_path(osc, dataset, &ctx.paths, project_root)?;
    let compressions = pbfhogg::parse_compressions(compression_str, false)?;
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

#[allow(clippy::too_many_arguments)]
fn cmd_bench_all(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    _features: &[String],
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "bench all")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let effective = build_root.unwrap_or(project_root);
    pbfhogg::bench_all::run(&ctx.harness, &ctx.paths, effective, &pbf_path, file_mb, runs, dataset)
}

// ---------------------------------------------------------------------------
// Bench commands (elivagar-specific)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_bench_eliv_self(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    skip_to: Option<&str>,
    no_ocean: bool,
    compression_level: Option<u32>,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, None, &feat_refs, true, "bench self")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
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
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "bench planetiler")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    elivagar::bench_planetiler::run(
        &ctx.harness,
        &pbf_path,
        file_mb,
        runs,
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        project_root,
    )
}

fn cmd_bench_tilemaker(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "bench tilemaker")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    elivagar::bench_tilemaker::run(
        &ctx.harness,
        &pbf_path,
        file_mb,
        runs,
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        project_root,
    )
}

#[allow(clippy::too_many_arguments)]
fn cmd_bench_eliv_all(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    _features: &[String],
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "bench all")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let effective = build_root.unwrap_or(project_root);
    elivagar::bench_all::run(
        &ctx.harness,
        &ctx.paths,
        effective,
        &pbf_path,
        file_mb,
        runs,
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
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
    let pi = bootstrap(None)?;
    elivagar::compare_tiles::run(&pi.target_dir, project_root, file_a, file_b, sample)
}

fn cmd_download_ocean(dev_config: &config::DevConfig, project: Project, project_root: &Path) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "download-ocean")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    elivagar::download_ocean::run(&paths.data_dir)
}

fn cmd_pmtiles_stats(files: &[String]) -> Result<(), DevError> {
    for file in files {
        pmtiles::run(file)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Verify commands (pbfhogg-specific)
// ---------------------------------------------------------------------------

fn cmd_verify(dev_config: &config::DevConfig, project: Project, project_root: &Path, build_root: Option<&Path>, verify: VerifyCommand) -> Result<(), DevError> {
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
            cmd_verify_pbfhogg(dev_config, project, project_root, build_root, verify)
        }
    }
}

fn cmd_verify_pbfhogg(dev_config: &config::DevConfig, _project: Project, project_root: &Path, build_root: Option<&Path>, verify: VerifyCommand) -> Result<(), DevError> {
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let harness = pbfhogg::verify::VerifyHarness::new(project_root, &pi.target_dir, build_root)?;

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
        Project::Elivagar => {
            // Micro-benchmark variants: build the example with hotpath and run it.
            if let Some(v) = variant {
                return match v {
                    "pmtiles" => {
                        project::require(project, Project::Elivagar, "hotpath pmtiles")?;
                        let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "hotpath pmtiles")?;
                        elivagar::bench_pmtiles::run_hotpath(&ctx.harness, &ctx.paths.scratch_dir, build_root.unwrap_or(project_root), tiles, runs, alloc)
                    }
                    "node-store" => {
                        project::require(project, Project::Elivagar, "hotpath node-store")?;
                        let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "hotpath node-store")?;
                        elivagar::bench_node_store::run_hotpath(&ctx.harness, &ctx.paths.scratch_dir, build_root.unwrap_or(project_root), nodes, runs, alloc)
                    }
                    other => Err(DevError::Config(format!(
                        "unknown hotpath variant '{other}' for elivagar (expected: pmtiles, node-store)"
                    ))),
                };
            }

            let ctx = BenchContext::new(dev_config, project, project_root, build_root, None, &all_features, true, "hotpath")?;
            let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
            let risk = if alloc { oom::MemoryRisk::AllocTracking } else { oom::MemoryRisk::Normal };
            oom::check_memory(file_mb, &risk, no_mem_check)?;
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
            let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("nidhogg"), &all_features, true, "hotpath")?;
            let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
            let risk = if alloc { oom::MemoryRisk::AllocTracking } else { oom::MemoryRisk::Normal };
            oom::check_memory(file_mb, &risk, no_mem_check)?;
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

            let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &all_features, true, "hotpath")?;
            let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
            let risk = if alloc { oom::MemoryRisk::AllocTracking } else { oom::MemoryRisk::Normal };
            oom::check_memory(file_mb, &risk, no_mem_check)?;
            let osc_path = resolve_osc_path(osc, dataset, &ctx.paths, project_root)?;

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
        Project::Elivagar => {
            let tool_name = tool.unwrap_or("perf");
            preflight::run_preflight(&preflight::profile_checks(tool_name))?;
            let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "profile")?;
            let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
            oom::check_memory(file_mb, &oom::MemoryRisk::AllocTracking, no_mem_check)?;
            let effective = build_root.unwrap_or(project_root);
            elivagar::profile::run(
                &ctx.harness,
                &pbf_path,
                file_mb,
                &ctx.paths.data_dir,
                &ctx.paths.scratch_dir,
                tool_name,
                no_ocean,
                features,
                effective,
            )
        }
        Project::Nidhogg => {
            let tool_name = tool.unwrap_or("perf");
            preflight::run_preflight(&preflight::profile_checks(tool_name))?;
            let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "profile")?;
            let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
            oom::check_memory(file_mb, &oom::MemoryRisk::AllocTracking, no_mem_check)?;

            let data_dir = ctx.paths
                .datasets
                .get(dataset)
                .and_then(|ds| ds.data_dir.as_ref())
                .map(|d| ctx.paths.data_dir.join(d))
                .unwrap_or_else(|| ctx.paths.data_dir.clone());

            nidhogg::profile::run(
                &ctx.harness,
                &pbf_path,
                file_mb,
                &data_dir,
                &ctx.paths.scratch_dir,
                tool_name,
                features,
                build_root.unwrap_or(project_root),
            )
        }
        _ => {
            project::require(project, Project::Pbfhogg, "profile")?;

            let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "profile")?;
            let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
            oom::check_memory(file_mb, &oom::MemoryRisk::AllocTracking, no_mem_check)?;
            let osc_path = resolve_osc_path(osc, dataset, &ctx.paths, project_root)?;

            // Try to get raw PBF path (optional).
            let pbf_raw_path = ctx.paths
                .datasets
                .get(dataset)
                .and_then(|ds| ds.pbf_raw.as_ref())
                .map(|raw_file| ctx.paths.data_dir.join(raw_file))
                .filter(|p| p.exists());

            pbfhogg::profile::run(
                &ctx.harness,
                &pbf_path,
                pbf_raw_path.as_deref(),
                &osc_path,
                dataset,
                file_mb,
                &ctx.paths.scratch_dir,
                features,
                build_root.unwrap_or(project_root),
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

    let pi = bootstrap(None)?;
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
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let data_dir_str = match data_dir {
        Some(d) => d.to_owned(),
        None => resolve_nidhogg_data_dir(dataset, &paths)?.display().to_string(),
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
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &paths, project_root)?;

    let data_dir = resolve_nidhogg_data_dir(dataset, &paths)?;

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

#[allow(clippy::too_many_arguments)]
fn cmd_bench_api(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    runs: usize,
    query: Option<&str>,
    _features: &[String],
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "bench api")?;
    let port = resolve_nidhogg_port(dev_config);

    // Resolve dataset PBF for metadata recording.
    let pbf_path = resolve_pbf_path(None, dataset, &ctx.paths, project_root).ok();
    let input_file = pbf_path.as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    let input_mb = pbf_path.as_ref().map(|p| file_size_mb(p)).transpose()?;

    nidhogg::bench_api::run(&ctx.harness, port, runs, query, input_file, input_mb)
}

#[allow(clippy::too_many_arguments)]
fn cmd_bench_nid_ingest(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("nidhogg"), &feat_refs, true, "bench ingest")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
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
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let port = resolve_nidhogg_port(dev_config);

    let data_dir_str = resolve_nidhogg_data_dir(dataset, &paths)?.display().to_string();

    let binary = build::cargo_build(&build::BuildConfig::release(Some("nidhogg")), project_root)?;
    nidhogg::verify_readonly::run(&binary, &data_dir_str, port, project_root)
}
