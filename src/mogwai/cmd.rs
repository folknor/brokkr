//! `brokkr mogwai` - build the CLI and run one frozen workload.

use crate::build;
use crate::config;
use crate::context::BenchContext;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::MeasureRequest;
use crate::output;
use crate::project::{self, Project};

use super::workload;

/// Run one registered workload, or list the registry when none is named.
pub(crate) fn run(req: &MeasureRequest, name: Option<&str>) -> Result<(), DevError> {
    project::require(req.project, Project::Mogwai, "mogwai")?;

    let cfg = workload::config(req.dev_config)?;

    // Bare-is-an-index, brokkr's standing shape for a command over a named set.
    let Some(name) = name else {
        output::bench_msg("registered workloads:");
        print!("{}", workload::format_index(cfg));
        return Ok(());
    };

    // Resolve before building: a typo'd name, a retired one, or a drifted
    // corpus should cost nothing. Hashing a multi-gigabyte delivery ahead of a
    // release build looks backwards, but `verify_file_hash` is mtime-cached, so
    // it is a stat on every run after the first.
    let hostname = crate::config::hostname()?;
    let resolved = workload::resolve(req.dev_config, &hostname, req.project_root, name)?;
    let args = resolved.args();

    if req.dry_run {
        output::bench_msg(&format!(
            "[dry-run] workload {} -> {}",
            resolved.name,
            args.join(" ")
        ));
        return Ok(());
    }

    let build_config = build::BuildConfig {
        package: cfg.package.clone(),
        bin: cfg.bin_target().map(str::to_owned),
        example: None,
        features: req.features.to_vec(),
        default_features: true,
        profile: "release",
    };

    let ctx = BenchContext::with_build_config(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        &build_config,
        "mogwai",
        req.force,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let binary_str = ctx.binary.display().to_string();
    let timing = resolved.entry.timing;

    // Filed under the workload name, following dellingr. Two fields are
    // deliberately empty rather than merely unused:
    //
    // - `input_file`, because a generated workload's "dataset" is part of its
    //   definition - it is seeded, not read - and duplicating the name into
    //   that column would only widen the table.
    // - `brokkr_args`, because it is a `pair_key` component. The expanded argv
    //   is recorded in `cli_args` where it stays visible to `brokkr results`
    //   and `--grep`, but keeping it out of the key is what lets a workload's
    //   rows keep pairing across a re-registration that did not change the
    //   work. The guard against a registration that DID change the work is the
    //   new-name rule, enforced at resolution, not the pairing key.
    // The identity-counter declaration travels WITH the run, rather than being
    // read from config at comparison time. A workload that re-declares its set
    // next month must not retroactively change what a comparison between two
    // old rows asserts - the rows were produced under the old declaration, and
    // that is the one their delta was judged against.
    //
    // Written even when the set is EMPTY, because its presence is what opts a
    // row into the counter contract at all: `--compare` diffs the undeclared
    // counters too, non-fatally, and it must do that only for rows that agreed
    // to be read that way. An elivagar row carrying `nodes=` in its kv did not.
    let metadata = vec![
        crate::db::KvPair::text(
            crate::db::IDENTITY_COUNTERS_KEY,
            resolved.entry.identity_counters.join(","),
        ),
        // Which clock this row's elapsed_ms came from. Two rows of the same
        // workload measured on different clocks are not comparable, and nothing
        // else on the row would say so.
        crate::db::KvPair::text(crate::db::TIMING_KEY, timing.as_str()),
    ];

    let config = BenchConfig {
        command: resolved.name.clone(),
        mode: None,
        // The corpus registry KEY, never the resolved path: the key is the same
        // on every host, so a corpus row stays comparable across machines. It
        // is also a `pair_key` component, which is right here - two runs over
        // different corpora are different benchmarks and must not pair.
        input_file: resolved.corpus().map(str::to_owned),
        input_mb: None,
        cargo_features: None,
        cargo_profile: build::CargoProfile::Release,
        runs: resolved.entry.runs.unwrap_or_else(|| req.runs()),
        cli_args: Some(harness::format_cli_args(&binary_str, &args)),
        brokkr_args: None,
        metadata,
    };

    output::bench_msg(&format!(
        "mogwai workload {}: {} run(s), {} timing",
        resolved.name,
        config.runs,
        timing.as_str()
    ));

    // Both paths scrape stderr counters; they differ only in which clock the
    // recorded wall comes from. Counters are what make any wall interpretable -
    // seeded work is bit-identical run to run, so a counter that moved means
    // the comparison is measuring two different jobs, not a faster one.
    match timing {
        // Brokkr's wall around the child. The default, because it needs nothing
        // inside mogwai: a baseline can be recorded at any commit whose CLI
        // still parses the frozen argv, including retroactively, since
        // `--compare` strips `--commit` from the pairing key.
        config::MogwaiTiming::External => {
            ctx.harness
                .run_external_with_counters(&config, &ctx.binary, &args, req.project_root)?;
        }
        // The target's own `elapsed_ms=`, for a workload whose setup is real
        // and is not the code under test. Strictly worse for provenance - there
        // is nothing to back-fill from before the commit that first emitted the
        // line - which is why the registry has to argue for it in
        // `timing_reason`. A missing `elapsed_ms=` is a hard error here, unlike
        // the external path where it is just another optional counter.
        config::MogwaiTiming::SelfReported => {
            ctx.harness
                .run_external_with_kv(&config, &ctx.binary, &args, req.project_root)?;
        }
    }

    Ok(())
}
