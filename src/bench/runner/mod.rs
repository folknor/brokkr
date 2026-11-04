//! `brokkr bench NAME`: one script-bench run, end to end.
//!
//! The run is one attempt with one document (`reference/bench-store.md`):
//! the build the bench's TOML asks for (host features folded in), in the
//! `--commit` worktree when given, then iterations, each a fresh Lua state
//! over one frozen compiled script, an owner lease and its children on the
//! resource protocol (`reference/bench-resources.md`), then one publication
//! to the per-user bench store. Whatever the attempt reached is published,
//! a failed build or an interrupted iteration included, and then the
//! command fails as the run did.

mod host;
mod stored_config;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::lua;
use super::store;
use crate::config::bench::{Bench, BenchScript};
use crate::error::DevError;
use crate::output;
use crate::project::Detection;

/// A parsed `brokkr bench` invocation.
pub(crate) struct BenchArgs {
    /// The bench to run; `None` lists the benches.
    pub(crate) name: Option<String>,
    /// Run the commit's build, in a persistent worktree.
    pub(crate) commit: Option<String>,
    pub(crate) iterations: std::num::NonZeroU32,
    /// Everything after NAME, unparsed: the bench's own params.
    pub(crate) params: Vec<String>,
}

/// The stdout and stderr preview a script sees of each child, in bytes.
pub(crate) const PREVIEW_BYTES: usize = 1024 * 1024;

/// How many times a publication whose commit errored is republished before
/// the command gives up and reports the run as possibly stored.
const REPUBLISH_ATTEMPTS: usize = 3;

pub(crate) fn run(
    args: &BenchArgs,
    detection: &Detection,
    invocation: &crate::db::RunInvocation,
) -> Result<(), DevError> {
    let benches = &detection.config.benches;
    let Some(name) = args.name.as_deref() else {
        list(benches);
        return Ok(());
    };
    let bench = benches.get(name).ok_or_else(|| {
        DevError::Refused(if benches.is_empty() {
            format!("no bench named {name}: this project's brokkr.toml declares none")
        } else {
            format!(
                "no bench named {name}; declared: {}",
                benches.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    })?;
    if args.params == ["--help"] {
        describe(bench, detection);
        return Ok(());
    }
    if !bench.params.is_empty() {
        return Err(DevError::Refused(format!(
            "bench {name} declares params ({}), which this brokkr cannot bind yet",
            bench.params.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    if let Some(extra) = args.params.first() {
        return Err(DevError::Usage(format!(
            "bench {name} takes no params, but {extra:?} follows its name; brokkr's own options go before the name"
        )));
    }
    let frozen = freeze(bench)?;
    let store_path = super::evidence::store_path()?;
    // Opened before anything is built or run: a run that cannot be stored is
    // never measured.
    let mut bench_store = super::evidence::open_store(&store_path)?;
    let started = Instant::now();
    let started_unix_ms = unix_ms();
    let uuid = crate::db::generate_uuid()?;
    let hostname = crate::config::hostname()?;
    let keep = crate::context::worktree_keep(&detection.config)?;
    let tree = crate::tree_policy::TreePolicy::of(&detection.config);
    // The whole run, worktree setup and publication included: a stop that
    // lands between two children, during the build or while publishing is
    // recorded rather than taking the default action, so the attempt is
    // still published as interrupted.
    let _interrupts = crate::shutdown::SigtermGuard::install();
    // An unknown revision is refused before anything is attempted; from here
    // on every outcome is published.
    let commit = crate::git::resolve_commit(
        detection.build_root.as_path(),
        args.commit.as_deref().unwrap_or("HEAD"),
    )?;
    let attempt = Attempt {
        detection,
        bench,
        frozen: &frozen,
        committed: args.commit.is_some(),
        uuid: &uuid,
        hostname: &hostname,
        started_unix_ms,
        iterations: args.iterations.get(),
        argv: crate::history::redact_secret_argv(&invocation.argv),
    };
    let mut document = attempt.document(&commit);
    let worktree = crate::context::with_worktree(
        Some(&detection.project),
        &detection.project_root,
        &detection.build_root,
        // The sha resolved above, so the tree built is the commit recorded
        // even if the ref moves meanwhile.
        args.commit.as_ref().map(|_| commit.as_str()),
        false,
        &tree,
        keep,
        |build_root| {
            attempt.execute(build_root.as_path(), &commit, &mut document);
            Ok(())
        },
    );
    if let Err(error) = worktree {
        // A git call cut short by the stop reports a spawn or subprocess
        // error, not an interrupt.
        document.outcome =
            if matches!(error, DevError::Interrupted) || crate::shutdown::should_unwind() {
                store::RunOutcome::Interrupted
            } else {
                store::RunOutcome::Failed {
                    reason: format!("worktree: {}", reason_of(&error)),
                }
            };
    }
    publish(&mut bench_store, &document)?;
    report(&document, &store_path, started);
    match &document.outcome {
        store::RunOutcome::Completed => Ok(()),
        store::RunOutcome::Interrupted => Err(DevError::Interrupted),
        store::RunOutcome::Failed { reason } => {
            output::error(&format!("bench {name} failed: {reason}"));
            Err(DevError::Reported(format!("bench {name} failed")))
        }
    }
}

/// The bare index: every bench, its target and where its script lives.
fn list(benches: &BTreeMap<String, Bench>) {
    if benches.is_empty() {
        output::bench_msg("this project's brokkr.toml declares no [bench.*] tables");
        return;
    }
    let mut lines = vec![format!(
        "{} declared:",
        output::count(benches.len(), "bench")
    )];
    for bench in benches.values() {
        lines.push(format!(
            "  {} (target {}, {})",
            bench.name,
            bench.target.as_deref().unwrap_or("[bin] default"),
            script_origin(bench)
        ));
    }
    lines.push(
        "run one with `brokkr bench <name>`; `brokkr bench <name> --help` describes it".into(),
    );
    output::bench_msg(&lines.join("\n"));
}

/// `brokkr bench NAME --help`: the bench's build settings and its params.
fn describe(bench: &Bench, detection: &Detection) {
    let mut lines = vec![
        format!("bench {}: {}", bench.name, script_origin(bench)),
        format!(
            "  target {}, profile {}, features [{}]{}",
            bench.target.as_deref().unwrap_or("[bin] default"),
            bench.profile.as_ref().map_or_else(
                || crate::runnables::build_profile(detection.config.bin.as_ref(), None).to_string(),
                ToString::to_string
            ),
            crate::command_args::resolve_features(&detection.config, &bench.features).join(", "),
            if bench.default_features {
                ""
            } else {
                ", no default features"
            }
        ),
    ];
    if bench.params.is_empty() {
        lines.push("  no params".into());
    }
    for (name, param) in &bench.params {
        let mut line = format!("  --{name}");
        if let Some(from) = &param.from {
            line.push_str(&format!(" (a key under {})", from.as_str()));
        }
        if let Some(default) = &param.default {
            line.push_str(&format!(" [default {default}]"));
        }
        if let Some(help) = &param.help {
            line.push_str(&format!(": {help}"));
        }
        lines.push(line);
    }
    output::bench_msg(&lines.join("\n"));
}

fn script_origin(bench: &Bench) -> String {
    match &bench.script {
        BenchScript::Inline { declared_in, .. } => {
            format!("inline script in {}", output::path(declared_in))
        }
        BenchScript::File { path, .. } => format!("script {}", output::path(path)),
    }
}

/// The script as this run executes it, read once: every iteration runs this
/// one compiled program, and the stored config records these bytes.
pub(super) struct Frozen {
    pub(super) bytes: Vec<u8>,
    /// The `.lua` file, or the config file holding the inline script.
    pub(super) origin: PathBuf,
    pub(super) script: lua::Script,
}

fn freeze(bench: &Bench) -> Result<Frozen, DevError> {
    let (bytes, origin, chunk) = match &bench.script {
        BenchScript::Inline {
            source,
            declared_in,
        } => (
            source.as_bytes().to_vec(),
            declared_in.clone(),
            format!("[bench.{}] script", bench.name),
        ),
        BenchScript::File { path, declared_in } => (
            std::fs::read(path).map_err(|e| {
                DevError::Config(format!(
                    "[bench.{}] script_file {} (set in {}) cannot be read: {e}",
                    bench.name,
                    path.display(),
                    declared_in.display()
                ))
            })?,
            path.clone(),
            path.display().to_string(),
        ),
    };
    let script = lua::load_script(&bytes, Some(&chunk)).map_err(|e| {
        DevError::Config(format!("[bench.{}] {}: {e}", bench.name, origin.display()))
    })?;
    Ok(Frozen {
        bytes,
        origin,
        script,
    })
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Everything one attempt is made of before it runs.
struct Attempt<'a> {
    detection: &'a Detection,
    bench: &'a Bench,
    frozen: &'a Frozen,
    committed: bool,
    uuid: &'a str,
    hostname: &'a str,
    started_unix_ms: i64,
    iterations: u32,
    argv: Vec<String>,
}

impl Attempt<'_> {
    /// The document as the attempt starts it: identity, conditions and the
    /// build asked for, its outcome interrupted until something decides it.
    fn document(&self, commit: &crate::git::Commit) -> store::RunDocument {
        let live = self.detection.build_root.as_path();
        let subject = crate::git::commit_subject(live, commit).ok();
        // A `--commit` worktree is always clean; the live tree under the
        // measurement policy, its config files exempt.
        let dirty = !self.committed
            && !crate::git::TreeState::read_for_measurement(
                live,
                &self.detection.config.included_config_files,
            )
            .is_some_and(|state| !state.for_measurement());
        let config = &self.detection.config;
        let profile = self.profile();
        let features = crate::command_args::resolve_features(config, &self.bench.features);
        let captured_env = config_env(config);
        store::RunDocument {
            format: store::DOCUMENT_FORMAT,
            uuid: self.uuid.to_owned(),
            project: self.detection.project.name().to_owned(),
            bench: self.bench.name.clone(),
            checkout: self.detection.build_root.as_path().display().to_string(),
            hostname: self.hostname.to_owned(),
            commit: commit.as_str().to_owned(),
            dirty,
            subject,
            started_unix_ms: self.started_unix_ms,
            brokkr_version: env!("CARGO_PKG_VERSION").to_owned(),
            argv: self.argv.clone(),
            conditions: store::Conditions {
                kernel: crate::host::kernel::collect().version,
                cpu_governor: crate::host::cpu::collect().governor,
                avail_memory_mb: i64::try_from(crate::host::memory::collect().available_mb).ok(),
                captured_env,
            },
            build: store::Build {
                target: self.bench.target.clone(),
                profile: profile.to_string(),
                features: features.clone(),
                default_features: self.bench.default_features,
                resolved: None,
                artifact: None,
            },
            params: BTreeMap::new(),
            config: stored_config::render(self.detection, &self.bench.name, self.frozen),
            iterations_requested: self.iterations,
            outcome: store::RunOutcome::Interrupted,
            iterations: Vec::new(),
        }
    }

    /// Build in `build_root` (the live checkout or the `--commit` worktree)
    /// and run, deciding the document's outcome.
    fn execute(
        &self,
        build_root: &Path,
        commit: &crate::git::Commit,
        document: &mut store::RunDocument,
    ) {
        let profile = self.profile();
        let features = document.build.features.clone();
        let program = match self.build(build_root, &profile, &features, document) {
            Ok(program) => program,
            Err(error) => {
                document.outcome =
                    if matches!(error, DevError::Interrupted) || crate::shutdown::should_unwind() {
                        store::RunOutcome::Interrupted
                    } else {
                        store::RunOutcome::Failed {
                            reason: format!("build: {}", reason_of(&error)),
                        }
                    };
                return;
            }
        };
        self.iterate(&program, commit, document);
    }

    fn profile(&self) -> crate::build::RustProfile {
        self.bench.profile.clone().unwrap_or_else(|| {
            crate::runnables::build_profile(self.detection.config.bin.as_ref(), None)
        })
    }

    /// Resolve and build the target, snapshot it, and fill in the document's
    /// build as far as it got. Returns the snapshot every child runs.
    fn build(
        &self,
        build_root: &Path,
        profile: &crate::build::RustProfile,
        features: &[String],
        document: &mut store::RunDocument,
    ) -> Result<PathBuf, DevError> {
        let config = &self.detection.config;
        let (package, target, example) = crate::runnables::resolve_build_target(
            build_root,
            config.bin.as_ref(),
            self.bench.target.as_deref(),
        )?;
        let build = crate::build::BuildConfig {
            package: Some(package),
            bin: (!example).then(|| target.clone()),
            example: example.then(|| target.clone()),
            features: features.to_vec(),
            default_features: self.bench.default_features,
            all_features: false,
            profile: profile.clone(),
        };
        let built = crate::build::cargo_build(&build, build_root)?;
        document.build.resolved = Some(store::ResolvedBuild {
            bin: built.target.clone(),
            features: built.unit_features.clone(),
        });
        let artifacts =
            crate::clean::layout::ProjectState::new(self.detection.project_root.as_path())
                .artifacts_dir();
        let (program, digest) = crate::runnables::snapshot_executable(&built.path, &artifacts)?;
        document.build.artifact = Some(digest);
        Ok(program)
    }

    /// Every iteration, stopping at the first that does not complete, and
    /// the run's outcome from them.
    fn iterate(
        &self,
        program: &Path,
        commit: &crate::git::Commit,
        document: &mut store::RunDocument,
    ) {
        let state = crate::clean::layout::ProjectState::new(self.detection.project_root.as_path());
        let mut since = host::Since::new(self.detection.build_root.as_path(), commit.clone());
        let total = self.iterations;
        for index in 0..total {
            if crate::shutdown::should_unwind() {
                document.outcome = store::RunOutcome::Interrupted;
                return;
            }
            let (iteration, returned) = host::run_iteration(host::IterationSetup {
                script: &self.frozen.script,
                program,
                cwd: self.detection.project_root.as_path(),
                state_root: state.root(),
                container: &state.bench_iterations_dir(),
                index: usize::try_from(index).unwrap_or(usize::MAX),
                since,
            });
            since = returned;
            let label = format!("iteration {}/{total}", index + 1);
            let outcome = iteration.outcome.clone();
            match (&outcome, iteration.wall_us) {
                (store::IterationOutcome::Completed { verdict }, Some(wall)) => {
                    let wall = std::time::Duration::from_micros(u64::try_from(wall).unwrap_or(0));
                    output::bench_msg(&format!(
                        "{label}: {}{}",
                        output::format_duration(wall),
                        verdict
                            .as_ref()
                            .map(|v| format!(", verdict {v:?}"))
                            .unwrap_or_default()
                    ));
                }
                (store::IterationOutcome::Failed { reason }, _) => {
                    output::warn(&format!("{label} failed: {reason}"));
                }
                _ => output::warn(&format!("{label} interrupted")),
            }
            document.iterations.push(iteration);
            match outcome {
                store::IterationOutcome::Completed { .. } => {}
                store::IterationOutcome::Failed { reason } => {
                    document.outcome = store::RunOutcome::Failed {
                        reason: format!("{label}: {reason}"),
                    };
                    return;
                }
                store::IterationOutcome::Interrupted => {
                    document.outcome = store::RunOutcome::Interrupted;
                    return;
                }
            }
        }
        let verdict_of = |i: &store::Iteration| match &i.outcome {
            store::IterationOutcome::Completed { verdict } => verdict.clone(),
            _ => None,
        };
        let first = document.iterations.first().and_then(verdict_of);
        document.outcome = if document.iterations.iter().all(|i| verdict_of(i) == first) {
            store::RunOutcome::Completed
        } else {
            let seen: Vec<String> = document
                .iterations
                .iter()
                .map(|i| verdict_of(i).map_or_else(|| "nil".to_owned(), |v| format!("{v:?}")))
                .collect();
            store::RunOutcome::Failed {
                reason: format!(
                    "the iterations disagree on the verdict: {}",
                    seen.join(", ")
                ),
            }
        };
    }
}

/// `capture_env` as the document stores it: credential values redacted.
fn config_env(config: &crate::config::DevConfig) -> BTreeMap<String, String> {
    super::evidence::redacted_env(&crate::config::captured_env_map(config))
}

/// An error's text for a document's failure reason, never empty.
fn reason_of(error: &DevError) -> String {
    let shown = error.to_string();
    if shown.trim().is_empty() {
        format!("{error:?}")
    } else {
        shown
    }
}

/// Publish `document`, republishing it while its commit's outcome is
/// unknown. Once an attempt is uncertain the run may be stored whatever
/// later attempts say, so only a success settles it.
fn publish(bench_store: &mut store::Store, document: &store::RunDocument) -> Result<(), DevError> {
    let mut uncertain: Option<store::StoreError> = None;
    for _ in 0..=REPUBLISH_ATTEMPTS {
        match bench_store.publish(document) {
            Ok(_) => return Ok(()),
            Err(error) if error.outcome_uncertain() => uncertain = Some(error),
            Err(error) if uncertain.is_none() => {
                return Err(DevError::Database(format!(
                    "run {} was not stored: {error}",
                    document.uuid
                )));
            }
            Err(_) => {}
        }
    }
    Err(DevError::Database(format!(
        "run {} may or may not have been stored: {}",
        document.uuid,
        uncertain.map_or_else(String::new, |e| e.to_string())
    )))
}

/// The run's one result line, never quiet-suppressed: its uuid is the only
/// handle `brokkr results` finds it by.
fn report(document: &store::RunDocument, store_path: &Path, started: Instant) {
    let outcome = match &document.outcome {
        store::RunOutcome::Completed => {
            let best = document
                .best_iteration()
                .and_then(|i| document.iterations[i].wall_us)
                .map_or_else(String::new, |us| {
                    format!(
                        ", best {} of {}",
                        output::format_duration(std::time::Duration::from_micros(
                            u64::try_from(us).unwrap_or(0)
                        )),
                        document.iterations.len()
                    )
                });
            format!("completed{best}")
        }
        store::RunOutcome::Failed { .. } => "failed".to_owned(),
        store::RunOutcome::Interrupted => "interrupted".to_owned(),
    };
    output::result_forced(&format!(
        "bench {}: run {} {outcome}{} in {} ({}); `brokkr results {}` shows it",
        document.bench,
        document.uuid,
        if document.dirty { ", dirty tree" } else { "" },
        output::format_duration(started.elapsed()),
        output::path(store_path),
        crate::db::short_uuid(&document.uuid),
    ));
}
