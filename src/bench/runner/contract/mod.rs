//! Contract tests: a project's own Lua test files, under `bench_tests`, that
//! run its bench scripts against scripted fixtures and assert what the
//! scripts did (`brokkr man bench`, Contract tests). No build, no data, no
//! process, no store.
//!
//! A test file runs in a State of its own (`lua::run_suite`) whose `run(spec)`
//! reaches [`Context::run`]: one arm of a declared bench, through the real
//! lifecycle up to the build. The params bind with the real binder against
//! the real resolved view of the project's composed config, every host
//! section replaced by the one the spec gives (held to the host schema a
//! config load applies); `validate()` runs on the real validate host;
//! `preflight()` and `bench()` run on virtual hosts that apply the real call
//! policy and answer every effect from the spec (`host`). A fixture the
//! script contradicts fails the test, never the script: it is the spec
//! that is wrong, or the script does what its test did not expect.
//!
//! What this proves is the script's behaviour given the observations the
//! spec declares. It does not prove that a program exists, that an input is
//! a file brokkr may read, that admission or cache preparation would pass
//! (inode identity, other leases), or anything about a real child's
//! markers: those are the real hosts', tested against real children.

mod host;
mod spec;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::{Frozen, freeze};
use crate::bench::lua::{self, Abort, Value};
use crate::bench::{FindingOwner, ScriptFinding, json, params, view};
use crate::config::bench::Bench;
use crate::error::DevError;

/// The hostname a contract test's view selects: its synthetic host.
pub(crate) const TEST_HOST: &str = "contract-test";

/// What every `run()` of a test session shares: the declared benches, the
/// composed config their view derives from, and the project root.
pub(crate) struct Context {
    pub(crate) benches: BTreeMap<String, Bench>,
    pub(crate) composed: toml::Table,
    pub(crate) project_root: PathBuf,
    /// `bench_lib`, which no test file may be a module of.
    pub(crate) library: Option<PathBuf>,
}

/// The file name a `binary()` result names the build by: the spec's
/// `program`, else the bench's `target`. A bench without a `target` has its
/// build found by cargo's runnable discovery, which a test, running no
/// process, does not do: its spec names the file.
fn program(spec: &spec::Spec, bench: &Bench) -> Result<String, String> {
    match (&spec.program, &bench.target) {
        (Some(given), Some(target)) if given != target => Err(format!(
            "the spec's program {given:?} is not [bench.{}]'s target {target:?}",
            bench.name
        )),
        (Some(given), _) => Ok(given.clone()),
        (None, Some(target)) => Ok(target.clone()),
        (None, None) => Err(format!(
            "[bench.{}] names no target, so its build is the one runnable discovery finds: give \
             the spec's program, the build's file name",
            bench.name
        )),
    }
}

/// A test file's driver: `run(spec)` against the context.
struct Driver {
    context: Arc<Context>,
}

impl lua::Driver for Driver {
    fn stop(&mut self) -> Result<(), Abort> {
        super::host::stopping()
    }
    fn stop_requested() -> bool {
        super::host::stop_pending()
    }
    fn run(&mut self, spec: Value) -> Result<Value, Abort> {
        self.context.run(spec)
    }
}

/// Run every test file `discover_tests` found, each finding a test file that
/// is also a bench script or a `bench_lib` module, a failed case, a file that
/// failed outside its cases, or one that declares none. A stop ends the run.
pub(crate) fn run_files(
    context: Context,
    files: &[PathBuf],
) -> Result<Vec<ScriptFinding>, DevError> {
    let libraries: std::collections::BTreeSet<&std::path::Path> = context
        .benches
        .values()
        .filter_map(|bench| bench.library.as_deref())
        .chain(context.library.as_deref())
        .collect();
    let mut findings = crate::bench::test_overlaps(files, &context.benches, &libraries);
    let context = Arc::new(context);
    for file in files {
        if crate::shutdown::should_unwind() {
            return Err(DevError::Interrupted);
        }
        let finding = |problem: String| ScriptFinding {
            owner: FindingOwner::Test,
            file: file.clone(),
            problem,
        };
        let source = match std::fs::read(file) {
            Ok(source) => source,
            Err(e) => {
                findings.push(finding(format!("cannot be read: {e}")));
                continue;
            }
        };
        let script = match lua::load_script(&source, Some(&file.display().to_string()), None) {
            Ok(script) => script,
            Err(error) => {
                findings.push(finding(error.to_string()));
                continue;
            }
        };
        let root = context.project_root.display().to_string();
        let ran = lua::run_suite(
            &script,
            &[("project_root", root)],
            Driver {
                context: Arc::clone(&context),
            },
        );
        for case in &ran.cases {
            if let Some(why) = &case.failure {
                findings.push(finding(format!("test {:?}: {why}", case.name)));
            }
        }
        match ran.ended {
            lua::SuiteEnd::Interrupted => return Err(DevError::Interrupted),
            lua::SuiteEnd::Failed(why) => findings.push(finding(why)),
            lua::SuiteEnd::Done if ran.cases.is_empty() => findings.push(finding(
                "declares no test case: wrap each in test(name, function() ... end)".into(),
            )),
            lua::SuiteEnd::Done => {}
        }
    }
    Ok(findings)
}

/// A fault of the test, never the script's: `run()` raises it in the test
/// file, failing the case that called it.
fn fixture_fault(why: impl Into<String>) -> Abort {
    Abort::Failed(why.into())
}

/// Where one `run()` stopped, and how.
struct Ended {
    stage: &'static str,
    outcome: &'static str,
    reason: Option<String>,
    verdict: Option<String>,
    record: Option<Value>,
}

impl Ended {
    fn at(stage: &'static str, outcome: &'static str, reason: Option<String>) -> Self {
        Self {
            stage,
            outcome,
            reason,
            verdict: None,
            record: None,
        }
    }
}

impl Context {
    /// One arm of the spec's bench, through bind, `validate()`,
    /// `preflight()` and `bench()`, as far as it gets: the result table,
    /// or the fault that fails the test.
    fn run(&self, spec: Value) -> Result<Value, Abort> {
        let spec = spec::parse(spec).map_err(|why| fixture_fault(format!("run(): {why}")))?;
        let bench = self.benches.get(&spec.bench).ok_or_else(|| {
            fixture_fault(format!(
                "run(): no bench named {}; declared: {}",
                spec.bench,
                self.benches.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
        })?;
        let view = self.view(spec.host.as_ref())?;
        let mut modules = std::collections::BTreeSet::new();
        let mut preflight_calls = Vec::new();
        let mut bench_calls = Vec::new();
        let ended = self.lifecycle(
            bench,
            &spec,
            &view,
            &mut modules,
            &mut preflight_calls,
            &mut bench_calls,
        )?;
        let mut fields = vec![
            ("stage".into(), Value::Text(ended.stage.into())),
            ("outcome".into(), Value::Text(ended.outcome.into())),
            ("calls".into(), Value::List(bench_calls)),
            ("preflight_calls".into(), Value::List(preflight_calls)),
            (
                "modules".into(),
                Value::List(modules.into_iter().map(Value::Text).collect()),
            ),
        ];
        if let Some(reason) = ended.reason {
            fields.push(("reason".into(), Value::Text(reason)));
        }
        if let Some(verdict) = ended.verdict {
            fields.push(("verdict".into(), Value::Text(verdict)));
        }
        if let Some(record) = ended.record {
            fields.push(("record".into(), record));
        }
        Ok(Value::Table(fields))
    }

    /// The view the spec's bench binds against: the composed config with
    /// every host section dropped and the spec's host, judged as a config
    /// load judges one, as the selected host.
    fn view(&self, host: Option<&toml::Table>) -> Result<view::View, Abort> {
        let mut composed: toml::Table = self
            .composed
            .iter()
            .filter(|(key, value)| *key != "host" && !crate::config::is_host_key(key, value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if let Some(host) = host {
            let section = toml::Value::Table(host.clone());
            crate::config::check_host_section(TEST_HOST, &section)
                .map_err(|e| fixture_fault(format!("run(): the spec's host: {e}")))?;
            let mut hosts = toml::Table::new();
            hosts.insert(TEST_HOST.into(), section);
            composed.insert("host".into(), toml::Value::Table(hosts));
        }
        Ok(view::build(&composed, TEST_HOST, &self.project_root))
    }

    #[allow(clippy::too_many_lines)] // The lifecycle in its order, each stage's ending beside it.
    fn lifecycle(
        &self,
        bench: &Bench,
        spec: &spec::Spec,
        view: &view::View,
        modules: &mut std::collections::BTreeSet<String>,
        preflight_calls: &mut Vec<Value>,
        bench_calls: &mut Vec<Value>,
    ) -> Result<Ended, Abort> {
        let refused = |stage, error: DevError| Ended::at(stage, "refused", Some(error.to_string()));
        let given = match params::parse(bench, &spec.params) {
            Ok(given) if given.help => {
                return Err(fixture_fault(
                    "run(): the params ask for --help, which describes a bench rather than \
                     running it",
                ));
            }
            Ok(given) => given,
            Err(error) => return unconsumed(spec, 0, 0).map(|()| refused("bind", error)),
        };
        let mut arms = match params::bind(bench, &given, view) {
            Ok(arms) => arms,
            Err(error) => return unconsumed(spec, 0, 0).map(|()| refused("bind", error)),
        };
        if arms.len() != 1 {
            return Err(fixture_fault(format!(
                "run(): the params bind {} arms ({}): a contract test runs one arm, so give \
                 every fanned-out param one value",
                arms.len(),
                params::arm_labels(&arms).join("; ")
            )));
        }
        let bound = arms.remove(0);
        // A contradiction is the spec's whatever the script calls; only an
        // absent name waits for a call that needs it.
        let program = program(spec, bench);
        if let (Err(why), Some(_)) = (&program, &spec.program) {
            return Err(fixture_fault(format!("run(): {why}")));
        }
        // Only an input exists before the iteration: its outputs, scratch
        // and spools are made in it.
        let inputs = bound.inputs();
        let root = std::path::Path::new(host::ITERATION_ROOT);
        if let Some(input) = inputs
            .iter()
            .find(|input| input.starts_with(root) || root.starts_with(input))
        {
            return Err(fixture_fault(format!(
                "run(): the input {} lies in {}, where the virtual iteration makes its outputs, \
                 scratch and spools",
                input.display(),
                host::ITERATION_ROOT
            )));
        }
        if let Some(path) = spec.files.keys().find(|path| {
            !inputs
                .iter()
                .any(|input| input.as_os_str() == path.as_str())
        }) {
            return Err(fixture_fault(format!(
                "run(): the spec's files gives {path}, which is no input this arm binds: files \
                 holds the inputs' contents, and the iteration makes everything else"
            )));
        }
        let frozen: Frozen = freeze(bench).map_err(|e| fixture_fault(format!("run(): {e}")))?;
        let config = Arc::new(view.lua());
        if let Some(why) = crate::bench::unvalidated(bench, frozen.script.has_validate()) {
            unconsumed(spec, 0, 0)?;
            return Ok(Ended::at(
                "validate",
                "refused",
                Some(format!("[bench.{}] {why}", bench.name)),
            ));
        }
        if frozen.script.has_validate() {
            let host = super::validate::ValidateHost {
                config: Arc::clone(&config),
            };
            let (ran, _) = lua::run_hook(
                lua::Hook::Validate,
                &frozen.script,
                &bound.preflight_args(),
                host,
            );
            modules.extend(ran.modules);
            if let Some(ended) = hook_ended("validate", ran.outcome, None)? {
                unconsumed(spec, 0, 0)?;
                return Ok(ended);
            }
        }
        let mut shared = host::Shared {
            since: spec.since.clone(),
            fault: None,
        };
        let mut preflight_used = 0;
        if frozen.script.has_preflight() {
            let host = host::VirtualPreflight {
                config: Arc::clone(&config),
                inputs: inputs.clone(),
                responses: spec.preflight_calls.clone(),
                page_cache: spec.page_cache.clone(),
                shared,
                observed: Vec::new(),
            };
            let (ran, host) = lua::run_hook(
                lua::Hook::Preflight,
                &frozen.script,
                &bound.preflight_args(),
                host,
            );
            modules.extend(ran.modules);
            preflight_calls.extend(host.observed);
            preflight_used = spec.preflight_calls.len() - host.responses.len();
            shared = host.shared;
            if let Some(ended) = hook_ended("preflight", ran.outcome, shared.fault.take())? {
                unconsumed(spec, preflight_used, 0)?;
                return Ok(ended);
            }
        }
        let iteration = host::VirtualIteration::new(
            Arc::clone(&config),
            program,
            &inputs,
            spec.calls.clone(),
            spec.files.clone(),
            shared,
        );
        let (sample, iteration) = lua::run_sample(&frozen.script, &bound.args(), iteration);
        modules.extend(sample.modules);
        bench_calls.extend(iteration.observed);
        if let Some(fault) = iteration.shared.fault {
            return Err(fixture_fault(fault));
        }
        let bench_used = spec.calls.len() - iteration.responses.len();
        unconsumed(spec, preflight_used, bench_used)?;
        Ok(match sample.outcome {
            lua::SampleOutcome::Interrupted => return Err(Abort::Interrupted),
            lua::SampleOutcome::Failed(reason) => Ended::at("bench", "failed", Some(reason)),
            lua::SampleOutcome::Completed(verdict) => {
                let record = match sample.record {
                    Some(record) => {
                        let text = serde_json::to_vec(&record).map_err(|e| {
                            fixture_fault(format!("run(): cannot hand back the record: {e}"))
                        })?;
                        Some(json::decode(&text).map_err(|e| {
                            fixture_fault(format!("run(): cannot hand back the record: {e}"))
                        })?)
                    }
                    None => None,
                };
                Ended {
                    stage: "bench",
                    outcome: "completed",
                    reason: None,
                    verdict,
                    record,
                }
            }
        })
    }
}

/// How a hook's outcome ends the run, `None` when it passed: a refusal or a
/// script error is the script's, a fault or brokkr's own failure the test's.
fn hook_ended(
    stage: &'static str,
    outcome: lua::HookOutcome,
    fault: Option<String>,
) -> Result<Option<Ended>, Abort> {
    if let Some(fault) = fault {
        return Err(fixture_fault(fault));
    }
    match outcome {
        lua::HookOutcome::Passed => Ok(None),
        lua::HookOutcome::Refused(why) => Ok(Some(Ended::at(stage, "refused", Some(why)))),
        lua::HookOutcome::Broken(why) => Ok(Some(Ended::at(stage, "broken", Some(why)))),
        lua::HookOutcome::Failed(why) => {
            Err(fixture_fault(format!("{stage}() could not run: {why}")))
        }
        lua::HookOutcome::Interrupted => Err(Abort::Interrupted),
    }
}

/// Refuse a run that left declared responses unused: a response nobody
/// asked for means the script skipped the call its test meant to exercise.
fn unconsumed(spec: &spec::Spec, preflight_used: usize, bench_used: usize) -> Result<(), Abort> {
    for (what, declared, used) in [
        (
            "preflight_calls",
            spec.preflight_calls.len(),
            preflight_used,
        ),
        ("calls", spec.calls.len(), bench_used),
    ] {
        if used < declared {
            return Err(fixture_fault(format!(
                "run(): the spec's {what} declares {declared} response{}, and the script made {used} \
                 such call{}: every declared response must be asked for",
                if declared == 1 { "" } else { "s" },
                if used == 1 { "" } else { "s" }
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
