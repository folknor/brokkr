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
mod request;
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
use request::{BenchRequest, HotpathRequest, ProfileRequest, ResultsQuery};
use resolve::results_db_path;

fn main() {
    let cli = Cli::parse();

    let result = run(cli);
    match result {
        Ok(()) => {}
        Err(DevError::ExitCode(code)) => process::exit(code),
        Err(e) => {
            output::error(&e.to_string());
            process::exit(1);
        }
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
        } => {
            let rq = ResultsQuery { query, commit, compare, compare_last, command, variant, limit, top };
            cmd_results(&project_root, &rq)
        }
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
            target,
            verbose,
            commit,
            features,
            dataset,
            variant,
            osc_seq,
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
                let feature = harness::hotpath_feature(alloc);
                let mut all_features: Vec<&str> = vec![feature];
                all_features.extend(features.iter().map(String::as_str));
                let req = HotpathRequest {
                    dev_config: &dev_config, project, project_root: &project_root, build_root,
                    dataset: &dataset, variant: &variant, runs,
                    all_features: &all_features, alloc, no_mem_check,
                };
                cmd_hotpath(&req, osc_seq.as_deref(), no_ocean, target.as_deref(), tiles, nodes)
            })
        }
        Command::Profile { verbose, commit, features, dataset, variant, osc_seq, tool, no_ocean, no_mem_check } => {
            output::set_quiet(!verbose);
            if features.iter().any(|f| f == "linux-io-uring") {
                preflight::run_preflight(&preflight::uring_checks())?;
            }
            with_worktree(&project_root, commit.as_deref(), |build_root| {
                let req = ProfileRequest {
                    dev_config: &dev_config, project, project_root: &project_root, build_root,
                    dataset: &dataset, variant: &variant, features: &features, no_mem_check,
                };
                cmd_profile(&req, osc_seq.as_deref(), tool.as_deref(), no_ocean)
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
        Command::Ingest { variant, dataset } => {
            let _lock = acquire_cmd_lock(project, &project_root, "ingest")?;
            nidhogg::cmd::ingest(&dev_config, project, &project_root, &variant, &dataset)
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
        // Print stderr (compilation warnings) first, stdout (test results) last
        // so the actionable failure details appear at the bottom of output.
        if !stderr.is_empty() {
            output::error(&stderr);
        }
        if !stdout.is_empty() {
            output::error(&stdout);
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
                return Err(DevError::ExitCode(code));
            }
            Ok(())
        }
    }
}

fn cmd_results(project_root: &Path, q: &ResultsQuery) -> Result<(), DevError> {
    let db_path = results_db_path(project_root);

    if !db_path.exists() {
        output::result_msg("no results yet (run a benchmark first)");
        return Ok(());
    }

    let results_db = db::ResultsDb::open(&db_path)?;

    if let Some(ref uuid_prefix) = q.query {
        let rows = results_db.query_by_uuid(uuid_prefix)?;
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
                    && let Some(report) = hotpath_fmt::format_hotpath_report(hotpath, q.top)
                {
                    println!("\n{report}");
                }
            }
        }
    } else if let Some(ref commits) = q.compare {
        let commit_a = commits.first().map_or("", String::as_str);
        let commit_b = commits.get(1).map_or("", String::as_str);
        let (rows_a, rows_b) = results_db.query_compare(commit_a, commit_b, q.command.as_deref(), q.variant.as_deref())?;
        let table = db::format_compare(commit_a, &rows_a, commit_b, &rows_b, q.top);
        println!("{table}");
    } else if q.compare_last {
        match results_db.query_compare_last(q.command.as_deref(), q.variant.as_deref())? {
            Some((commit_a, rows_a, commit_b, rows_b)) => {
                let table = db::format_compare(&commit_a, &rows_a, &commit_b, &rows_b, q.top);
                println!("{table}");
            }
            None => {
                output::result_msg("need at least two distinct commits to compare");
            }
        }
    } else {
        let filter = db::QueryFilter {
            commit: q.commit.clone(),
            command: q.command.clone(),
            variant: q.variant.clone(),
            limit: q.limit,
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
            output::lock_msg(&format!(
                "held by PID {}: {} {} ({})",
                info.pid, info.project, info.command, info.project_root,
            ));
        }
        None => {
            output::lock_msg("no active lock");
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
        BenchCommand::Commands { command, dataset, variant, osc_seq, runs } => {
            project::require(project, Project::Pbfhogg, "bench commands")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            pbfhogg::cmd::bench_commands(&req, &command, osc_seq.as_deref())
        }
        BenchCommand::Extract { dataset, variant, runs, bbox, strategies } => {
            project::require(project, Project::Pbfhogg, "bench extract")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            pbfhogg::cmd::bench_extract(&req, bbox.as_deref(), &strategies)
        }
        BenchCommand::Allocator { dataset, variant, runs } => {
            project::require(project, Project::Pbfhogg, "bench allocator")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            pbfhogg::cmd::bench_allocator(&req)
        }
        BenchCommand::BlobFilter { dataset, indexed_variant, raw_variant, runs } => {
            project::require(project, Project::Pbfhogg, "bench blob-filter")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &indexed_variant, runs, features };
            pbfhogg::cmd::bench_blob_filter(&req, &raw_variant)
        }
        BenchCommand::Planetiler { dataset, variant, runs } => {
            project::require(project, Project::Pbfhogg, "bench planetiler")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            pbfhogg::cmd::bench_planetiler(&req)
        }
        BenchCommand::Read { dataset, variant, runs, modes } => {
            project::require(project, Project::Pbfhogg, "bench read")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            pbfhogg::cmd::bench_read(&req, &modes)
        }
        BenchCommand::Write { dataset, variant, runs, compression } => {
            project::require(project, Project::Pbfhogg, "bench write")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            pbfhogg::cmd::bench_write(&req, &compression)
        }
        BenchCommand::Merge { dataset, variant, osc_seq, runs, uring, compression } => {
            project::require(project, Project::Pbfhogg, "bench merge")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            pbfhogg::cmd::bench_merge(&req, osc_seq.as_deref(), uring, &compression)
        }
        BenchCommand::All { dataset, variant, runs } => {
            project::require(project, Project::Pbfhogg, "bench all")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            pbfhogg::cmd::bench_all(&req)
        }

        // ----- elivagar bench variants -----
        BenchCommand::ElivSelf { dataset, variant, runs, skip_to, no_ocean, compression_level } => {
            project::require(project, Project::Elivagar, "bench self")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            elivagar::cmd::bench_self(&req, skip_to.as_deref(), no_ocean, compression_level)
        }
        BenchCommand::NodeStore { nodes, runs } => {
            project::require(project, Project::Elivagar, "bench node-store")?;
            elivagar::cmd::bench_node_store(dev_config, project, project_root, build_root, nodes, runs)
        }
        BenchCommand::Pmtiles { tiles, runs } => {
            project::require(project, Project::Elivagar, "bench pmtiles")?;
            elivagar::cmd::bench_pmtiles(dev_config, project, project_root, build_root, tiles, runs)
        }
        BenchCommand::ElivPlanetiler { dataset, variant, runs } => {
            project::require(project, Project::Elivagar, "bench eliv-planetiler")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            elivagar::cmd::bench_planetiler(&req)
        }
        BenchCommand::Tilemaker { dataset, variant, runs } => {
            project::require(project, Project::Elivagar, "bench tilemaker")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            elivagar::cmd::bench_tilemaker(&req)
        }
        BenchCommand::ElivAll { dataset, variant, runs } => {
            project::require(project, Project::Elivagar, "bench eliv-all")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            elivagar::cmd::bench_all(&req)
        }

        // ----- nidhogg bench variants -----
        BenchCommand::Api { dataset, runs, query } => {
            project::require(project, Project::Nidhogg, "bench api")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: "raw", runs, features };
            nidhogg::cmd::bench_api(&req, query.as_deref())
        }
        BenchCommand::NidIngest { dataset, variant, runs } => {
            project::require(project, Project::Nidhogg, "bench ingest")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            nidhogg::cmd::bench_ingest(&req)
        }
        BenchCommand::Tiles { dataset, tiles, variant, runs, uring } => {
            project::require(project, Project::Nidhogg, "bench tiles")?;
            let req = BenchRequest { dev_config, project, project_root, build_root, dataset: &dataset, variant: &variant, runs, features };
            nidhogg::cmd::bench_tiles(&req, &tiles, uring)
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

fn cmd_hotpath(req: &HotpathRequest, osc_seq: Option<&str>, no_ocean: bool, target: Option<&str>, tiles: usize, nodes: usize) -> Result<(), DevError> {
    if target.is_some() && req.project != Project::Elivagar {
        return Err(DevError::Config(
            "hotpath targets (pmtiles, node-store) are only available for elivagar".into(),
        ));
    }

    match req.project {
        Project::Elivagar => elivagar::cmd::hotpath(req, target, tiles, nodes, no_ocean),
        Project::Nidhogg => nidhogg::cmd::hotpath(req),
        _ => {
            project::require(req.project, Project::Pbfhogg, "hotpath")?;
            pbfhogg::cmd::hotpath(req, osc_seq)
        }
    }
}

fn cmd_profile(req: &ProfileRequest, osc_seq: Option<&str>, tool: Option<&str>, no_ocean: bool) -> Result<(), DevError> {
    match req.project {
        Project::Elivagar => elivagar::cmd::profile(req, tool, no_ocean),
        Project::Nidhogg => nidhogg::cmd::profile(req, tool),
        _ => {
            project::require(req.project, Project::Pbfhogg, "profile")?;
            pbfhogg::cmd::profile(req, osc_seq)
        }
    }
}
