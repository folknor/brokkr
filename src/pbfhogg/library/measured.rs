//! `brokkr library SCENARIO --bench N` (`docs/commands/library.md`,
//! Measuring a scenario): N whole processes of one scenario, each verified
//! as a verification is and agreeing with the first, recorded as one group
//! of rows (the scenario's and one per call) under one uuid, with every
//! process's trajectory in sidecar.db under the same uuid.
//!
//! `--hotpath N` and `--alloc N` (Profiling a scenario) are the same loop on
//! an instrumented harness: each process's valid hotpath report is part of
//! its verdict, and the group is the scenario row alone, carrying the chosen
//! process's report, since the profile is the process's, not a call's.
//!
//! Publication order: each process's trajectory is stored as soon as it is
//! judged (so brokkr holds at most one in memory), with the session's
//! metadata in the same transaction; after the last process the metadata is
//! final; the results.db group transaction comes last and is what publishes
//! the measurement. Any failure before it stores no row, and the sidecar
//! session is filed under the `dirty` alias and named.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::counterpart::Reuse;
use super::describe::{RoleSource, Scenario};
use super::identity::{self, Identity};
use super::run::{self, Measured, Places};
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::measure::GroupRow;
use crate::measure::execution::Iteration;
use crate::output;
use crate::pbfhogg::binary::{LibraryHarness, PbfhoggBinary};

/// What one measured process came to, kept for the rows (its trajectory
/// is already stored).
struct Process {
    wall: std::time::Duration,
    /// Each call's `return_us - entry_us`, by call.
    call_walls: BTreeMap<String, i64>,
    snapshots: BTreeMap<String, BTreeMap<String, u64>>,
    peak_rss_kb: i64,
    identity: Identity,
    ending: crate::output::Ending,
    /// Its hotpath report, in a profiled measurement.
    hotpath: Option<crate::db::HotpathData>,
    /// Its CLI counterpart's command interval and sampled peak RSS, in a
    /// library against CLI measurement.
    cli: Option<CliSample>,
}

/// One timed CLI process of a library against CLI measurement.
#[derive(Debug, Clone, Copy)]
struct CliSample {
    interval_us: i64,
    peak_rss_kb: i64,
}

/// A library against CLI measurement's second sidecar session, holding each
/// CLI process's trajectory as the run of its iteration.
struct CliSession {
    uuid: String,
    command: String,
    /// The fastest interval so far and its iteration.
    best: Option<(usize, i64)>,
    stored: usize,
}

/// The handles and the session a measurement runs with.
pub(crate) struct Measurement<'a> {
    pub(crate) harness: &'a LibraryHarness,
    pub(crate) cli: &'a PbfhoggBinary,
    pub(crate) session: &'a BenchHarness,
    /// Bench, hotpath or alloc, with the process count.
    pub(crate) mode: crate::measure::MeasureMode,
    pub(crate) dataset: &'a str,
    pub(crate) project_root: &'a Path,
}

/// Measure `scenario`, its dataset roles bound to `datasets`.
#[allow(clippy::too_many_lines)]
pub(crate) fn measure(
    bench: &Measurement<'_>,
    scenario: &Scenario,
    datasets: &BTreeMap<String, PathBuf>,
    places: &Places<'_>,
) -> Result<(), DevError> {
    let session = bench.session;
    let runs = bench.mode.runs();
    let profiled = bench.mode.profiled();
    let mut dataset_ids = BTreeMap::new();
    for (role, path) in datasets {
        dataset_ids.insert(role.clone(), identity::dataset(path, bench.project_root)?);
    }
    let build = bench.harness.build_identity().cloned();
    let features = build
        .as_ref()
        .and_then(|b| b.features.clone())
        .unwrap_or_default();
    let build_profile = build.as_ref().map(|b| match &b.profile {
        crate::build::BuildKind::Cargo(profile) => profile.label().to_owned(),
        other => format!("{other:?}"),
    });
    if let Some(build) = build {
        session.note_build(build);
    }
    let command = format!("library:{}", scenario.id);
    let uuid = crate::db::generate_uuid()?;
    let short = crate::db::short_uuid(&uuid);
    let sidecar = session.open_sidecar()?;
    let start_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let info = |ending: Option<crate::output::Ending>| crate::db::sidecar::RunInfo {
        run_start_epoch: Some(start_epoch),
        pid: None,
        command: Some(command.clone()),
        binary_path: None,
        binary_xxh128: Some(bench.harness.digest().to_owned()),
        git_commit: Some(session.commit().clone()),
        mode: session.effective_mode(),
        dataset: Some(bench.dataset.to_owned()),
        ending,
    };
    // The session exists before its first process, so a crash leaves it
    // identifiable, with what was stored.
    sidecar.store_meta(&uuid, 0, 0, Some(&info(None)), true)?;
    // A library against CLI measurement keeps every CLI process's trajectory
    // in a session of its own, which the `/cli` row names.
    let isolation_call = scenario
        .calls
        .first()
        .filter(|_| scenario.isolation && profiled.is_none());
    let mut cli_session = match isolation_call {
        Some(call) => {
            let session = CliSession {
                uuid: crate::db::generate_uuid()?,
                command: format!("{command}/{}{}", call.id, super::vs_cli::CLI_SUFFIX),
                best: None,
                stored: 0,
            };
            sidecar.store_meta(
                &session.uuid,
                0,
                0,
                Some(&cli_info(info(None), &session, bench.cli.digest(), None)),
                true,
            )?;
            Some(session)
        }
        None => None,
    };

    let config = BenchConfig {
        command: command.clone(),
        input_file: None,
        input_mb: None,
        cargo_features: None,
        external_build: None,
        runs: runs.get(),
        cli_args: None,
        metadata: Vec::new(),
        arm_qualifier: None,
    };
    let mut baseline: Option<Identity> = None;
    let mut reuse = Reuse::default();
    let mut processes: Vec<Process> = Vec::new();
    // The default run a reader of the session sees: the fastest passing
    // process so far (ties to the lowest index), else the last one stored.
    let mut best: Option<(usize, std::time::Duration, crate::output::Ending)> = None;
    let mut stored = 0usize;
    let measured = session.measure(
        &config,
        None,
        crate::measure::wall::WallSource::Closure,
        false,
        |i, _| {
            let mut trajectory = None;
            let mut cli_trajectory = None;
            let verdict = run::verify(
                bench.harness,
                bench.cli,
                scenario,
                datasets,
                places,
                Some(Measured {
                    iteration: i,
                    baseline: baseline.as_ref(),
                    reuse: &mut reuse,
                    datasets: &dataset_ids,
                    features: &features,
                    profiled,
                }),
                &mut trajectory,
                &mut cli_trajectory,
            );
            let passed = verdict.as_ref().is_ok_and(|r| r.passed);
            // The CLI process's trajectory, stored as this iteration's run of
            // the CLI session whatever the verdict, as the library's is; its
            // sample only from a valid interval.
            let mut cli_sample = None;
            if let (Some(session), Some(data)) = (cli_session.as_mut(), cli_trajectory) {
                let interval = verdict
                    .as_ref()
                    .ok()
                    .and_then(|r| r.cli.as_ref())
                    .map(|(_, timed)| timed.interval.wall_us());
                if let Some(interval_us) = interval {
                    cli_sample = Some(CliSample {
                        interval_us,
                        peak_rss_kb: data.summary.vm_hwm_kb,
                    });
                    if passed && session.best.is_none_or(|(_, us)| interval_us < us) {
                        session.best = Some((i, interval_us));
                    }
                }
                let best_idx = session.best.map_or(i, |(idx, _)| idx);
                let meta_info = cli_info(
                    info(None),
                    session,
                    bench.cli.digest(),
                    Some(data.ending.clone()),
                );
                sidecar
                    .store_session_run(
                        &session.uuid,
                        i,
                        &data,
                        &crate::db::sidecar::SessionMeta {
                            best_run_idx: best_idx,
                            total_runs: session.stored + 1,
                            run_info: Some(&meta_info),
                            sidecar_only: true,
                        },
                    )
                    .map_err(|e| {
                        DevError::Verify(format!(
                            "iteration {i}'s CLI trajectory could not be stored in sidecar.db: {e}"
                        ))
                    })?;
                session.stored += 1;
            }
            if passed && cli_session.is_some() && cli_sample.is_none() {
                return Err(DevError::Internal(format!(
                    "iteration {i} passed without its CLI command interval"
                )));
            }
            if let Some(data) = trajectory {
                let ending = data.ending.clone();
                let wall = verdict
                    .as_ref()
                    .map_or(std::time::Duration::ZERO, |r| r.wall);
                let candidate = if passed {
                    match &best {
                        Some((_, w, _)) if *w <= wall => best.clone(),
                        _ => Some((i, wall, ending.clone())),
                    }
                } else {
                    Some((i, wall, ending.clone()))
                };
                let (best_idx, _, best_ending) = candidate.clone().unwrap_or((i, wall, ending));
                let meta_info = info(Some(best_ending));
                sidecar
                    .store_session_run(
                        &uuid,
                        i,
                        &data,
                        &crate::db::sidecar::SessionMeta {
                            best_run_idx: best_idx,
                            total_runs: stored + 1,
                            run_info: Some(&meta_info),
                            sidecar_only: true,
                        },
                    )
                    .map_err(|e| {
                        DevError::Verify(format!(
                            "iteration {i}'s trajectory could not be stored in sidecar.db: {e}"
                        ))
                    })?;
                stored += 1;
                best = candidate;
                if passed
                    && let Ok(report) = &verdict
                    && let Some(identity) = &report.identity
                {
                    processes.push(Process {
                        wall: report.wall,
                        call_walls: report
                            .spans
                            .iter()
                            .map(|s| (s.call.clone(), s.return_us - s.entry_us))
                            .collect(),
                        snapshots: report.snapshots.clone(),
                        peak_rss_kb: data.summary.vm_hwm_kb,
                        identity: identity.clone(),
                        ending: data.ending.clone(),
                        hotpath: report.hotpath.clone(),
                        cli: cli_sample,
                    });
                }
            }
            let report = verdict?;
            if !report.passed {
                super::show_report(scenario, &report);
                return Err(DevError::Verify(format!(
                    "library scenario {} failed at iteration {i} ({})",
                    scenario.id,
                    output::count(report.failures.len(), "failure")
                )));
            }
            // The next process's preparation evicts nothing of this one's: a
            // passing run directory that could not be removed would leave its
            // outputs, digest-warmed, in memory before the next library arm.
            // Both trajectories are stored by now.
            if cli_session.is_some() && report.kept {
                return Err(DevError::Verify(format!(
                    "iteration {i}'s run directory {} could not be removed, so the next \
                     process's page-cache preparation cannot hold",
                    output::path(&report.run_dir)
                )));
            }
            if i == 0 {
                baseline.clone_from(&report.identity);
            }
            let mut iteration = Iteration::from_result(BenchResult {
                elapsed_ms: crate::measure::wall::elapsed_to_ms(&report.wall),
                elapsed_us: Some(crate::measure::wall::elapsed_to_us(&report.wall)),
                kv: Vec::new(),
                iterations: Vec::new(),
                distribution: None,
                hotpath: None,
            });
            iteration.wall = Some(report.wall);
            Ok(iteration)
        },
    );
    let cli_session = cli_session;
    let cli = cli_session.as_ref();
    let mut measurement = match measured {
        Ok(measurement) => measurement,
        Err(e) => return Err(file_failed(session, &sidecar, &uuid, stored, cli, e)),
    };

    let failed = measurement.error.take().or_else(|| {
        measurement
            .iterations
            .iter_mut()
            .find_map(|i| i.error.take())
    });
    let winner = measurement.winner.filter(|_| failed.is_none());
    let (Some(winner), true) = (winner, processes.len() == runs.get()) else {
        return Err(file_failed(
            session,
            &sidecar,
            &uuid,
            stored,
            cli,
            failed.unwrap_or_else(|| DevError::Verify("the measurement made no sample".into())),
        ));
    };

    // The final metadata names the chosen process, before anything is
    // published.
    let chosen = &processes[winner];
    let cli_side = match (isolation_call, cli) {
        (Some(call), Some(cli)) => Some(CliSide {
            call,
            session: cli,
            qualifier: super::vs_cli::qualifier(
                scenario,
                call.cli.as_ref().ok_or_else(|| {
                    DevError::Internal("an isolation call lost its counterpart".into())
                })?,
                &dataset_ids,
            )
            .map_err(|e| DevError::Internal(format!("the CLI workload qualifier: {e}")))?,
            library_build: library_build(build_profile.as_deref(), &features),
        }),
        _ => None,
    };
    let published = (|| -> Result<bool, DevError> {
        sidecar.store_meta(
            &uuid,
            winner,
            stored,
            Some(&info(Some(chosen.ending.clone()))),
            true,
        )?;
        let (scenario_row, mut call_rows) =
            rows(scenario, &config, &processes, winner, datasets, profiled)?;
        if let Some(side) = &cli_side {
            let (row, cli_winner) = cli_row(&call_rows, &processes, side)?;
            sidecar.store_meta(
                &side.session.uuid,
                cli_winner,
                side.session.stored,
                // Every CLI process of a passing measurement exited 0.
                Some(&cli_info(
                    info(None),
                    side.session,
                    bench.cli.digest(),
                    Some(crate::output::Ending::Exited(0)),
                )),
                true,
            )?;
            call_rows.push(row);
        }
        // The publication boundary: a stop seen up to here stores no row.
        if crate::shutdown::should_unwind() {
            return Err(DevError::Interrupted);
        }
        session.record_group(
            &uuid,
            &scenario_row,
            &call_rows,
            &chosen.identity.documents(),
        )
    })();
    let stored_rows = match published {
        Ok(stored_rows) => stored_rows,
        Err(e) => return Err(file_failed(session, &sidecar, &uuid, stored, cli, e)),
    };
    if let Some(side) = &cli_side {
        let comparison = super::vs_cli::Comparison {
            call: side.call.id.clone(),
            library_us: processes
                .iter()
                .map(|p| p.call_walls.get(&side.call.id).copied().unwrap_or(0))
                .collect(),
            cli_us: processes
                .iter()
                .map(|p| p.cli.map_or(0, |c| c.interval_us))
                .collect(),
            library_build: side.library_build.clone(),
            policy_differences: policy_differences(side.call),
        };
        for line in comparison.lines() {
            output::result_msg(&line);
        }
        output::sidecar_msg(&format!(
            "CLI trajectories: {} in sidecar.db {} (`brokkr sidecar {}`)",
            output::count(side.session.stored, "process"),
            crate::db::short_uuid(&side.session.uuid),
            crate::db::short_uuid(&side.session.uuid)
        ));
    }
    if !stored_rows {
        // A dirty tree: the trajectories are kept, filed as dirty.
        if let Err(e) = sidecar.set_latest("dirty", &uuid) {
            output::warn(&format!(
                "could not alias the sidecar session as dirty: {e}"
            ));
        }
    }
    output::sidecar_msg(&format!(
        "{} stored, best run {}/{} (sidecar.db{})",
        output::count(stored, "process"),
        winner + 1,
        stored,
        if stored_rows {
            String::new()
        } else {
            format!(" {short}, filed as dirty")
        }
    ));
    drop(sidecar);
    session.backup_sidecar();
    Ok(())
}

/// What a library against CLI measurement's `/cli` row is built from.
struct CliSide<'a> {
    call: &'a super::describe::Call,
    session: &'a CliSession,
    /// The CLI workload's arm qualifier ([`super::vs_cli::qualifier`]).
    qualifier: String,
    library_build: String,
}

/// The CLI session's run info: the library session's, under the `/cli`
/// command and the CLI executable's digest.
fn cli_info(
    library: crate::db::sidecar::RunInfo,
    session: &CliSession,
    cli_digest: &str,
    ending: Option<crate::output::Ending>,
) -> crate::db::sidecar::RunInfo {
    crate::db::sidecar::RunInfo {
        command: Some(session.command.clone()),
        binary_xxh128: Some(cli_digest.to_owned()),
        ending,
        ..library
    }
}

/// The harness build as the comparison names it, in the CLI build's
/// spelling ([`super::vs_cli::CLI_BUILD`]) so equal builds read equal.
fn library_build(profile: Option<&str>, features: &str) -> String {
    format!(
        "{}, {}",
        profile.unwrap_or("release"),
        if features.is_empty() {
            "default features".to_owned()
        } else {
            format!("features {features}")
        }
    )
}

/// The counterpart's declared policy differences, joined.
fn policy_differences(call: &super::describe::Call) -> String {
    call.cli
        .as_ref()
        .map(|c| c.policy_differences.join("; "))
        .unwrap_or_default()
}

/// The `/cli` row beside its call row in `call_rows`: every CLI interval in
/// iteration order, its headline the fastest (ties to the lowest index)
/// with that process's sampled peak RSS, under the CLI workload's own
/// qualifier. The chosen iteration is returned too.
fn cli_row(
    call_rows: &[GroupRow],
    processes: &[Process],
    side: &CliSide<'_>,
) -> Result<(GroupRow, usize), DevError> {
    let call_command = side
        .session
        .command
        .strip_suffix(super::vs_cli::CLI_SUFFIX)
        .unwrap_or(&side.session.command);
    let call_row = call_rows
        .iter()
        .find(|r| r.config.command == call_command)
        .ok_or_else(|| DevError::Internal(format!("no call row {call_command}")))?;
    let samples: Vec<CliSample> = processes
        .iter()
        .map(|p| {
            p.cli
                .ok_or_else(|| DevError::Internal("a measured process has no CLI interval".into()))
        })
        .collect::<Result<_, _>>()?;
    let mut winner = 0;
    for (i, s) in samples.iter().enumerate() {
        if s.interval_us < samples[winner].interval_us {
            winner = i;
        }
    }
    let us: Vec<i64> = samples.iter().map(|s| s.interval_us).collect();
    let ms: Vec<i64> = us
        .iter()
        .map(|u| crate::measure::wall::us_to_ms(*u))
        .collect();
    let meta = |key: &str| crate::db::KvKey::metadata(key);
    let mut metadata = vec![
        KvPair::text(meta("library.identity"), &side.qualifier),
        KvPair::int(
            meta("library.cli_iteration"),
            i64::try_from(winner).unwrap_or(i64::MAX),
        ),
        KvPair::text(meta("library.cli_sidecar"), &side.session.uuid),
        KvPair::text(
            meta(super::vs_cli::INTERVAL_KEY),
            super::interval::DEFINITION,
        ),
        KvPair::text(meta("library.prep"), super::prep::PROCEDURE),
        KvPair::text(meta("library.build"), &side.library_build),
        KvPair::text(meta("library.cli_build"), super::vs_cli::CLI_BUILD),
    ];
    let policy = policy_differences(side.call);
    if !policy.is_empty() {
        metadata.push(KvPair::text(meta("library.policy_differences"), policy));
    }
    let row = GroupRow {
        config: BenchConfig {
            command: side.session.command.clone(),
            input_file: call_row.config.input_file.clone(),
            input_mb: call_row.config.input_mb,
            // The CLI's own build, release with default features, never the
            // session's harness build (whose profile, features and
            // provenance would otherwise be recorded on this row and split
            // a CLI trend by the harness's profile).
            cargo_features: Some(String::new()),
            external_build: Some(crate::build::BuildKind::Cargo(
                crate::build::RustProfile::Release,
            )),
            runs: call_row.config.runs,
            cli_args: None,
            metadata,
            arm_qualifier: Some(side.qualifier.clone()),
        },
        result: BenchResult {
            elapsed_ms: ms[winner],
            elapsed_us: Some(us[winner]),
            kv: vec![KvPair::int(
                "peak_rss_kb_sampled",
                samples[winner].peak_rss_kb,
            )],
            iterations: ms,
            distribution: None,
            hotpath: None,
        },
        iterations_us: us,
    };
    Ok((row, winner))
}

/// A measurement that stores no row: its sidecar session is aliased dirty
/// and named, whatever the alias write did, and a CLI session is named
/// beside it (one alias cannot hold both).
fn file_failed(
    session: &BenchHarness,
    sidecar: &crate::db::sidecar::SidecarDb,
    uuid: &str,
    stored: usize,
    cli: Option<&CliSession>,
    error: DevError,
) -> DevError {
    if let Some(cli) = cli {
        let short = crate::db::short_uuid(&cli.uuid);
        output::error(&format!(
            "the measurement's CLI trajectories ({} stored) are in sidecar.db {short} \
             (`brokkr sidecar {short}`)",
            cli.stored
        ));
    }
    let short = crate::db::short_uuid(uuid);
    if let Err(e) = sidecar.set_latest("dirty", uuid) {
        output::warn(&format!(
            "could not alias the sidecar session as dirty: {e}"
        ));
    }
    // Named whatever was stored: the session exists from before the first
    // process, so even an empty one is found by its id.
    output::error(&format!(
        "no results row stored; {stored} of the measurement's process trajectories are in \
         sidecar.db {short} (`brokkr sidecar {short}`)"
    ));
    if stored > 0 {
        session.backup_sidecar();
    }
    error
}

/// The rows of a passing measurement: the scenario's, then one per call in
/// describe order, each carrying the chosen process's identity. A profiled
/// measurement has the scenario row alone, with the chosen process's
/// report: the profile is the process's, never a call's.
fn rows(
    scenario: &Scenario,
    config: &BenchConfig,
    processes: &[Process],
    winner: usize,
    datasets: &BTreeMap<String, PathBuf>,
    profiled: Option<crate::measure::ProfiledMode>,
) -> Result<(GroupRow, Vec<GroupRow>), DevError> {
    let chosen = &processes[winner];
    let qualifier = chosen.identity.qualifier();
    let features = (!chosen.identity.features.is_empty()).then(|| chosen.identity.features.clone());
    let (input_file, input_mb) = input(scenario, datasets);
    let meta = |extra: &[(&str, &str)]| {
        let mut kv = vec![
            KvPair::text(
                crate::db::KvKey::metadata("library.plan"),
                &chosen.identity.plan.digest,
            ),
            KvPair::text(crate::db::KvKey::metadata("library.identity"), &qualifier),
            KvPair::int(
                crate::db::KvKey::metadata("library.iteration"),
                i64::try_from(winner).unwrap_or(i64::MAX),
            ),
        ];
        for (key, value) in extra {
            kv.push(KvPair::text(crate::db::KvKey::metadata(*key), *value));
        }
        kv
    };
    let row_config = |command: String, metadata: Vec<KvPair>| BenchConfig {
        command,
        input_file: input_file.clone(),
        input_mb,
        cargo_features: features.clone(),
        external_build: None,
        runs: config.runs,
        cli_args: None,
        metadata,
        arm_qualifier: Some(qualifier.clone()),
    };
    let walls = |us: Vec<i64>| -> (Vec<i64>, Vec<i64>) {
        let ms = us
            .iter()
            .map(|u| crate::measure::wall::us_to_ms(*u))
            .collect();
        (ms, us)
    };
    let (ms, us) = walls(
        processes
            .iter()
            .map(|p| crate::measure::wall::elapsed_to_us(&p.wall))
            .collect(),
    );
    let scenario_row = GroupRow {
        config: row_config(config.command.clone(), meta(&[])),
        result: BenchResult {
            elapsed_ms: ms[winner],
            elapsed_us: Some(us[winner]),
            kv: vec![KvPair::int("peak_rss_kb", chosen.peak_rss_kb)],
            iterations: ms,
            distribution: None,
            hotpath: chosen.hotpath.clone(),
        },
        iterations_us: us,
    };
    if profiled.is_some() {
        if scenario_row.result.hotpath.is_none() {
            return Err(DevError::Internal(
                "a profiled measurement chose a process without its report".into(),
            ));
        }
        return Ok((scenario_row, Vec::new()));
    }
    let mut calls = Vec::new();
    for call in &scenario.calls {
        let per: Vec<i64> = processes
            .iter()
            .map(|p| p.call_walls.get(&call.id).copied().unwrap_or(0))
            .collect();
        let context = chosen
            .identity
            .contexts
            .get(&call.id)
            .map(|d| d.digest.clone())
            .unwrap_or_default();
        let (ms, us) = walls(per);
        let kv = chosen
            .snapshots
            .get(&call.id)
            .map(snapshot_kv)
            .unwrap_or_default();
        calls.push(GroupRow {
            config: row_config(
                format!("{}/{}", config.command, call.id),
                meta(&[("library.context", &context)]),
            ),
            result: BenchResult {
                elapsed_ms: ms[winner],
                elapsed_us: Some(us[winner]),
                kv,
                iterations: ms,
                distribution: None,
                hotpath: None,
            },
            iterations_us: us,
        });
    }
    Ok((scenario_row, calls))
}

/// A sealed call's snapshot as its row's counters, each `snapshot.<name>`
/// (a namespace no brokkr-owned key shares, so no counter can overwrite or
/// impersonate one), exact: an integer when it fits, else its decimal text.
fn snapshot_kv(snapshot: &BTreeMap<String, u64>) -> Vec<KvPair> {
    snapshot
        .iter()
        .map(|(name, value)| {
            let key = format!("snapshot.{name}");
            match i64::try_from(*value) {
                Ok(v) => KvPair::int(key, v),
                Err(_) => KvPair::text(key, value.to_string()),
            }
        })
        .collect()
}

/// The row's input: the bound file's name for a scenario with one dataset
/// role (with its size), the names joined with `+` in role order for
/// several, none for none.
fn input(
    scenario: &Scenario,
    datasets: &BTreeMap<String, PathBuf>,
) -> (Option<String>, Option<f64>) {
    let files: Vec<&PathBuf> = scenario
        .roles
        .iter()
        .filter(|r| matches!(r.source, RoleSource::Dataset { .. }))
        .filter_map(|r| datasets.get(&r.id))
        .collect();
    let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned());
    match files.as_slice() {
        [] => (None, None),
        [one] => {
            #[allow(clippy::cast_precision_loss)]
            let mb = std::fs::metadata(one)
                .ok()
                .map(|m| m.len() as f64 / 1_000_000.0);
            (name(one), mb)
        }
        many => (
            Some(
                many.iter()
                    .filter_map(|p| name(p))
                    .collect::<Vec<_>>()
                    .join("+"),
            ),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn process(wall_ms: u64, call_us: i64) -> Process {
        let doc = |kind: &str, text: &str| {
            identity::Document::of(
                kind,
                &super::super::json::parse("d", text.as_bytes()).unwrap(),
            )
            .unwrap()
        };
        Process {
            wall: std::time::Duration::from_millis(wall_ms),
            call_walls: BTreeMap::from([("a".to_owned(), call_us)]),
            snapshots: BTreeMap::from([(
                "a".to_owned(),
                BTreeMap::from([
                    ("writer_blocks".to_owned(), 3),
                    ("big".to_owned(), u64::MAX),
                    ("meta.library.identity".to_owned(), 1),
                ]),
            )]),
            peak_rss_kb: 2048,
            identity: Identity {
                plan: doc("plan", r#"{"calls":{"a":{"n":1}}}"#),
                contexts: BTreeMap::from([("a".to_owned(), doc("context", r#"{"p":1}"#))]),
                datasets: BTreeMap::new(),
                generated: BTreeMap::new(),
                seeds: BTreeMap::new(),
                prep: None,
                features: String::new(),
            },
            ending: crate::output::Ending::Exited(0),
            hotpath: None,
            cli: None,
        }
    }

    #[test]
    fn the_rows_carry_the_chosen_process_and_every_iteration_in_order() {
        let scenario = super::super::describe::parse(
            br#"{"protocol": 1, "scenarios": [{"id": "s", "description": "d", "deadline_s": 60,
              "roles": [], "calls": [{"id": "a", "label": "l"}], "assertions": []}]}"#,
        )
        .unwrap()
        .scenarios
        .remove(0);
        let config = BenchConfig {
            command: "library:s".to_owned(),
            input_file: None,
            input_mb: None,
            cargo_features: None,
            external_build: None,
            runs: 2,
            cli_args: None,
            metadata: Vec::new(),
            arm_qualifier: None,
        };
        let processes = [process(30, 700), process(20, 900)];
        let (scenario_row, calls) =
            rows(&scenario, &config, &processes, 1, &BTreeMap::new(), None).unwrap();
        assert_eq!(scenario_row.config.command, "library:s");
        assert_eq!(scenario_row.result.iterations, [30, 20]);
        assert_eq!(scenario_row.iterations_us, [30_000, 20_000]);
        assert_eq!(scenario_row.result.elapsed_us, Some(20_000));
        let qualifier = processes[1].identity.qualifier();
        assert_eq!(
            scenario_row.config.arm_qualifier.as_deref(),
            Some(qualifier.as_str())
        );
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.config.command, "library:s/a");
        assert_eq!(call.iterations_us, [700, 900]);
        assert_eq!(call.result.elapsed_us, Some(900));
        let keys: Vec<&str> = call.result.kv.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "snapshot.big",
                "snapshot.meta.library.identity",
                "snapshot.writer_blocks"
            ],
            "every snapshot name stays a counter, none a brokkr-owned key"
        );
        assert_eq!(call.result.kv[0].value.to_string(), u64::MAX.to_string());
        let meta: Vec<&str> = call
            .config
            .metadata
            .iter()
            .map(|p| p.key.as_str())
            .collect();
        assert_eq!(
            meta,
            [
                "meta.library.plan",
                "meta.library.identity",
                "meta.library.iteration",
                "meta.library.context"
            ]
        );
    }

    #[test]
    fn the_cli_row_carries_its_own_fastest_interval_qualifier_and_session() {
        let scenario = super::super::describe::parse(
            br#"{"protocol": 1, "scenarios": [{"id": "s", "description": "d", "deadline_s": 60,
              "isolation": true,
              "roles": [{"id": "input", "kind": "pbf", "source": "dataset", "variant": "indexed"},
                        {"id": "out", "kind": "pbf", "source": "output", "owner": "a"}],
              "calls": [{"id": "a", "label": "l",
                         "cli": {"argv": ["cat", {"input": "input"}, "-o", {"output": "out"}],
                                 "policy_differences": ["the CLI raises NOFILE"]}}],
              "assertions": []}]}"#,
        )
        .unwrap()
        .scenarios
        .remove(0);
        let config = BenchConfig {
            command: "library:s".to_owned(),
            input_file: None,
            input_mb: None,
            cargo_features: None,
            external_build: None,
            runs: 3,
            cli_args: None,
            metadata: Vec::new(),
            arm_qualifier: None,
        };
        let mut processes = [process(30, 700), process(20, 900), process(25, 800)];
        for (p, (interval_us, peak_rss_kb)) in
            processes.iter_mut().zip([(500, 10), (400, 20), (400, 30)])
        {
            p.cli = Some(CliSample {
                interval_us,
                peak_rss_kb,
            });
        }
        let (_, call_rows) =
            rows(&scenario, &config, &processes, 1, &BTreeMap::new(), None).unwrap();
        let session = CliSession {
            uuid: "cli-session".to_owned(),
            command: "library:s/a/cli".to_owned(),
            best: None,
            stored: 3,
        };
        let datasets = BTreeMap::from([("input".to_owned(), "xxh128:1".to_owned())]);
        let side = CliSide {
            call: &scenario.calls[0],
            session: &session,
            qualifier: super::super::vs_cli::qualifier(
                &scenario,
                scenario.calls[0].cli.as_ref().unwrap(),
                &datasets,
            )
            .unwrap(),
            library_build: library_build(Some("release"), ""),
        };
        let (row, winner) = cli_row(&call_rows, &processes, &side).unwrap();
        // The fastest interval, ties to the lowest index: process 1, not
        // the library's chosen process by accident.
        assert_eq!(winner, 1);
        assert_eq!(row.config.command, "library:s/a/cli");
        assert_eq!(row.iterations_us, [500, 400, 400]);
        assert_eq!(row.result.elapsed_us, Some(400));
        assert_eq!(row.result.kv[0].key.as_str(), "peak_rss_kb_sampled");
        let qualifier = row.config.arm_qualifier.unwrap();
        assert!(
            qualifier.contains(r#""inputs":{"input":"xxh128:1"}"#)
                && qualifier.contains(r#""interval":"cli-interval-v1""#)
                && !qualifier.contains("plan"),
            "{qualifier}"
        );
        let meta: Vec<String> = row
            .config
            .metadata
            .iter()
            .map(|p| format!("{}={}", p.key.as_str(), p.value))
            .collect();
        for expected in [
            "meta.library.cli_iteration=1",
            "meta.library.cli_sidecar=cli-session",
            "meta.library.build=release, default features",
            "meta.library.policy_differences=the CLI raises NOFILE",
        ] {
            assert!(meta.contains(&expected.to_owned()), "{expected}: {meta:?}");
        }
    }

    #[test]
    fn a_profiled_measurement_is_the_scenario_row_with_the_chosen_report() {
        let scenario = super::super::describe::parse(
            br#"{"protocol": 1, "scenarios": [{"id": "s", "description": "d", "deadline_s": 60,
              "hotpath": true, "roles": [], "calls": [{"id": "a", "label": "l"}],
              "assertions": []}]}"#,
        )
        .unwrap()
        .scenarios
        .remove(0);
        let config = BenchConfig {
            command: "library:s".to_owned(),
            input_file: None,
            input_mb: None,
            cargo_features: None,
            external_build: None,
            runs: 2,
            cli_args: None,
            metadata: Vec::new(),
            arm_qualifier: None,
        };
        let profile = |name: &str| {
            crate::db::hotpath_data_from_json(&serde_json::json!({"functions_timing": {"data": [
                {"name": name, "calls": 1, "total": "1 ms"}]}}))
            .unwrap()
        };
        let mut processes = [process(30, 700), process(20, 900)];
        processes[0].hotpath = Some(profile("slow"));
        processes[1].hotpath = Some(profile("fast"));
        let mode = Some(crate::measure::ProfiledMode::Hotpath);
        let (row, calls) = rows(&scenario, &config, &processes, 1, &BTreeMap::new(), mode).unwrap();
        assert!(
            calls.is_empty(),
            "no call rows: the profile is the process's"
        );
        assert_eq!(row.config.command, "library:s");
        assert_eq!(row.result.hotpath.unwrap().functions[0].name, "fast");
        processes[1].hotpath = None;
        assert!(rows(&scenario, &config, &processes, 1, &BTreeMap::new(), mode).is_err());
    }
}
