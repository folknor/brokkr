//! `brokkr corpus` - the piners parity-corpus runner.
//!
//! Resolves a keyword-selected slice of the pinned corpus, hard-verifies
//! every selected probe's `strategy.pine` + `tv_trades.csv` against the
//! read-only submodule, writes a manifest, builds the harness once, and
//! invokes it with `--manifest <path>`. The harness consumes the manifest
//! and emits NDJSON disposition lines that brokkr renders.
//!
//! Verification is the only hard correctness gate today: a missing path or
//! a hash mismatch aborts before anything runs. Parity tiers are read from
//! the harness output but do not fail the run yet - tier-based pass/fail
//! arrives with the deferred parity-baseline work. The run fails only on a
//! real break (`compile_fail`/`runtime_fail`), a non-zero harness exit, or
//! a hash mismatch.
//!
//! The harness binary itself lives in piners (declared via
//! `[piners.harness]`). Until it learns `--manifest`, the spawn captures
//! its "unknown flag" failure faithfully; the brokkr-side plumbing here is
//! structurally complete.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::artefacts::ArtefactDir;
use crate::config::DevConfig;
use crate::error::DevError;
use crate::lockfile::{self, LockContext};
use crate::output;
use crate::piners::manifest::Manifest;
use crate::piners::registry::{self, Registry};
use crate::piners::report;
use crate::piners::select::{self, SelectArgs};
use crate::ratatoskr::build;

/// Where corpus run dirs live, relative to the project root:
/// `<this>/corpus/run-N/`.
const ARTEFACT_PARENT: &str = ".brokkr/piners";

/// Flags lifted off the `Corpus` CLI command.
#[derive(Debug, Default)]
pub struct CorpusArgs {
    pub keywords: Vec<String>,
    pub probe: Option<String>,
    pub all: bool,
    pub verify_only: bool,
    /// Stamp `pins.toml` from the corpus filesystem instead of running.
    /// Routed to [`crate::piners::reseed`]; see its module docs.
    pub reseed: bool,
    /// `Some(true)` = debug, `Some(false)` = release, `None` = default
    /// (debug for this command).
    pub profile_override: Option<bool>,
    pub keep_artefacts: bool,
}

/// Entry point for `brokkr corpus`.
#[allow(clippy::too_many_lines)] // linear orchestration: load, select, verify, build, run, report
pub fn corpus(
    project_root: &Path,
    dev_config: &DevConfig,
    args: &CorpusArgs,
) -> Result<(), DevError> {
    let cfg = dev_config.piners.clone().unwrap_or_default();

    // Reseed stamps pins.toml from the corpus filesystem - no registry to
    // load (it may not exist yet), no build, no harness. Route early.
    if args.reseed {
        return crate::piners::reseed::run(project_root, &cfg, args);
    }

    let registry_dir = project_root.join(cfg.registry_dir());
    let registry = Registry::load(&registry_dir)?;
    registry.lint()?;

    let sel_args = SelectArgs {
        keywords: args.keywords.clone(),
        probe: args.probe.clone(),
        all: args.all,
        verify_only: args.verify_only,
    };
    let ids = select::resolve(&registry, &sel_args)?;

    // Hard correctness gate: verify every selected pin before running.
    let corpus_root = project_root.join(cfg.corpus_root());
    output::corpus_msg(&format!(
        "verifying {} probe(s) against {}",
        ids.len(),
        corpus_root.display()
    ));
    let mut verified = Vec::with_capacity(ids.len());
    for id in &ids {
        let pin = registry.pins.get(id).ok_or_else(|| {
            DevError::Config(format!("piners: internal: selected id '{id}' absent from pins"))
        })?;
        verified.push(registry::verify_probe(id, pin, &corpus_root, project_root)?);
    }

    if args.verify_only {
        output::corpus_msg(&format!("verify-only: {} probe(s) OK", verified.len()));
        return Ok(());
    }

    let harness_cfg = cfg.harness.as_ref().ok_or_else(|| {
        DevError::Config(
            "corpus: no [piners.harness] section in brokkr.toml. \
             Declare `[piners.harness]` with `package = \"<crate>\"` \
             (and optional `binary`, `features`, `debug`)."
                .into(),
        )
    })?;

    // Feed paths resolve relative to brokkr.toml; handed through verbatim.
    let feeds: BTreeMap<String, PathBuf> = cfg
        .feeds
        .iter()
        .map(|(k, v)| (k.clone(), project_root.join(v)))
        .collect();

    let project_root_str = project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "piners",
        command: "corpus",
        project_root: &project_root_str,
    })?;
    let _sigterm = crate::shutdown::SigtermGuard::install();

    // Default profile is debug: parity is opt-level-independent, and the
    // debug build keeps the edit/run loop inside the cache-warm window.
    let debug = args
        .profile_override
        .unwrap_or_else(|| harness_cfg.debug.unwrap_or(true));
    let built = build::build_for_harness(
        project_root,
        harness_cfg,
        debug,
        Some(&|pid| _lock.set_child_pid(pid)),
        Some(&|| _lock.clear_child_pid()),
        true,
    )?;
    output::corpus_msg(&format!(
        "harness build ok (features={}, binary={})",
        built.features_label,
        built.binary.display()
    ));

    let artefact_parent = project_root.join(ARTEFACT_PARENT);
    let artefacts = ArtefactDir::allocate(&artefact_parent, "corpus", args.keep_artefacts)?;

    let manifest_path = artefacts.path().join("manifest.json");
    let manifest = Manifest::build(&corpus_root, &verified, &registry, feeds);
    manifest.write(&manifest_path)?;
    output::corpus_msg(&format!(
        "manifest: {} probe(s) -> {}",
        verified.len(),
        manifest_path.display()
    ));

    // No enforced ceiling: the ~270s budget is a guideline, not a wall.
    // PID is tracked so `brokkr kill` reaches the harness.
    let binary_str = built.binary.display().to_string();
    let manifest_str = manifest_path.display().to_string();
    let artefact_str = artefacts.path().display().to_string();
    let bin_dir_str = built.bin_dir.display().to_string();
    let env_pairs: Vec<(&str, &str)> = vec![
        ("BROKKR_HARNESS_ARTEFACT_DIR", &artefact_str),
        ("BROKKR_TEST_BIN_DIR", &bin_dir_str),
    ];

    let capture = match output::run_captured_with_env_and_deadline(
        &binary_str,
        &["--manifest", &manifest_str],
        project_root,
        &env_pairs,
        Duration::MAX,
        Some(&|pid| _lock.set_child_pid(pid)),
        true,
    ) {
        Ok(c) => c,
        Err(DevError::Interrupted) => {
            _lock.clear_child_pid();
            artefacts.finalize_failure();
            return Err(DevError::Interrupted);
        }
        Err(e) => {
            std::fs::write(
                artefacts.path().join("spawn-error.txt"),
                format!("failed to spawn {}: {e}\n", built.binary.display()),
            )
            .ok();
            artefacts.finalize_failure();
            return Err(e);
        }
    };
    _lock.clear_child_pid();

    let captured = capture.captured;
    std::fs::write(artefacts.path().join("harness.stdout"), &captured.stdout).ok();
    std::fs::write(artefacts.path().join("harness.stderr"), &captured.stderr).ok();

    let report = report::parse(&captured.stdout);
    report::render(&report);

    let elapsed_ms = captured.elapsed.as_millis();

    // The harness exit code is authoritative (see docs/commands/corpus.md):
    //   0 = clean, 1 = compile_fail/runtime_fail break(s), 2 = harness
    //   error (bad manifest, unreadable feeds). Anything else (signal,
    //   unexpected code) is a failure too.
    match captured.status.code() {
        Some(0) => {
            output::corpus_msg(&format!("PASS in {elapsed_ms}ms"));
            artefacts.finalize_success()?;
            Ok(())
        }
        other => {
            let dir = artefacts.path().to_path_buf();
            artefacts.finalize_failure();
            let reason = match other {
                Some(1) => "parity break(s)".to_owned(),
                Some(2) => "harness error".to_owned(),
                Some(c) => format!("harness exit={c}"),
                None => "harness killed by signal".to_owned(),
            };
            output::corpus_msg(&format!(
                "FAIL: {reason} in {elapsed_ms}ms (artefacts: {})",
                dir.display()
            ));
            Err(DevError::ExitCode(1))
        }
    }
}
