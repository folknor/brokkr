//! The virtual hosts a contract test runs a script's `preflight()` and
//! `bench()` on: the call policy the real hosts apply (`crate::bench::policy`),
//! with every effect answered from the test's fixtures instead of the disk,
//! a process or git. A fixture the script's behaviour contradicts (a call
//! nobody declared a response for, a `stopped_at` it never asked for, a file
//! it was given no contents for) is a fault of the test, kept apart in
//! `fault` so it can never read as the script's own failure.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use super::super::PREVIEW_BYTES;
use super::super::host::{stop_pending, stopping};
use super::spec::{Ending, PreflightEnding, PreflightResponse, Response};
use crate::bench::lua::{
    self, Abort, CallOptions, CallOutcome, HandleRef, StartFailure, Value, Word,
};
use crate::bench::policy::{self, Calls, Declared};
use crate::bench::resources::{Handle, Origin};
use crate::reservations::{Access, NameArea, Names};

/// Where a virtual iteration's outputs, scratch and spools live: a path
/// nothing on disk holds, the same on every host.
pub(super) const ITERATION_ROOT: &str = "/brokkr-contract/iteration";

/// What both virtual hosts share: the `since()` answers and the fault.
#[derive(Debug, Default)]
pub(super) struct Shared {
    pub(super) since: BTreeMap<String, bool>,
    /// The first fixture fault: once set, the test fails with it whatever
    /// the script did next.
    pub(super) fault: Option<String>,
}

impl Shared {
    /// Record `why` as the test's fault and end the hook or sample.
    fn fault(&mut self, why: String) -> Abort {
        let shown = format!("the contract test's fixtures: {why}");
        if self.fault.is_none() {
            self.fault = Some(why);
        }
        Abort::Machinery(shown)
    }

    fn since(&mut self, sha: &str) -> Result<bool, Abort> {
        stopping()?;
        match self.since.get(sha) {
            Some(answer) => Ok(*answer),
            None => Err(self.fault(format!(
                "the script asked since({sha:?}), and the spec's since gives no answer for it"
            ))),
        }
    }
}

/// The file name a call's program is shown by, as the real hosts show it.
fn short(program: &str) -> String {
    Path::new(program)
        .file_name()
        .map_or_else(|| program.to_owned(), |n| n.to_string_lossy().into_owned())
}

/// The first `PREVIEW_BYTES` of `bytes`, and whether that cut it.
fn preview(bytes: &[u8]) -> (Vec<u8>, bool) {
    let cut = bytes.len().min(PREVIEW_BYTES);
    (bytes[..cut].to_vec(), cut < bytes.len())
}

/// One `preflight()` call as observed.
fn observed_preflight(call: &policy::PreflightCall) -> Value {
    Value::Table(vec![
        ("program".into(), Value::Text(call.program.clone())),
        (
            "argv".into(),
            Value::List(call.argv.iter().cloned().map(Value::Text).collect()),
        ),
        ("env".into(), env_table(&call.env)),
    ])
}

fn env_table(env: &[(String, String)]) -> Value {
    Value::Table(
        env.iter()
            .map(|(name, value)| (name.clone(), Value::Text(value.clone())))
            .collect(),
    )
}

fn paths(handles: &[Handle]) -> Value {
    Value::List(
        handles
            .iter()
            .map(|h| Value::Text(h.path().display().to_string()))
            .collect(),
    )
}

/// The builtins' side of one arm's `preflight()` in a contract test.
pub(super) struct VirtualPreflight {
    pub(super) config: Arc<Value>,
    pub(super) inputs: Vec<PathBuf>,
    pub(super) responses: VecDeque<PreflightResponse>,
    pub(super) page_cache: BTreeMap<String, String>,
    pub(super) shared: Shared,
    pub(super) observed: Vec<Value>,
}

fn bench_only(name: &str) -> Abort {
    Abort::Failed(format!("{name}() is only available inside bench()"))
}

impl lua::Host for VirtualPreflight {
    fn stop(&mut self) -> Result<(), Abort> {
        stopping()
    }
    fn stop_requested() -> bool {
        stop_pending()
    }
    fn begin_block(&mut self, _at: Instant) {}

    fn binary(&mut self, words: Vec<Word>, options: CallOptions) -> Result<CallOutcome, Abort> {
        stopping()?;
        let call = policy::declare_preflight(words, options)?;
        self.observed.push(observed_preflight(&call));
        let Some(response) = self.responses.pop_front() else {
            return Err(self.shared.fault(format!(
                "preflight() made call {} ({}), and the spec's preflight_calls declares only {}",
                self.observed.len(),
                call.program,
                self.observed.len() - 1
            )));
        };
        let program = short(&call.program);
        let (stdout, stdout_truncated) = preview(&response.stdout);
        let (stderr, stderr_truncated) = preview(&response.stderr);
        let mut outcome = CallOutcome {
            program: program.clone(),
            exit: None,
            signal: None,
            stdout,
            stdout_truncated,
            stderr,
            stderr_truncated,
            stderr_tail: None,
            stdout_file: None,
            stderr_file: None,
            hotpath: None,
            stopped_at: None,
            start_error: None,
        };
        match response.ending {
            PreflightEnding::Exit(code) => outcome.exit = Some(code),
            PreflightEnding::Signal(signal) => outcome.signal = Some(signal),
            PreflightEnding::StartError { operation, message } => {
                outcome.program = call.program.clone();
                outcome.start_error = Some(StartFailure {
                    operation,
                    errno: None,
                    message: format!("{operation}: {}: {message}", call.program),
                });
            }
        }
        Ok(outcome)
    }

    fn since(&mut self, sha: &str) -> Result<bool, Abort> {
        self.shared.since(sha)
    }
    fn output(&mut self, _: &str) -> Result<HandleRef, Abort> {
        Err(bench_only("output"))
    }
    fn scratch(&mut self, _: &str) -> Result<HandleRef, Abort> {
        Err(bench_only("scratch"))
    }
    fn config(&mut self) -> Result<Arc<Value>, Abort> {
        stopping()?;
        Ok(Arc::clone(&self.config))
    }
    fn read(&mut self, _: usize) -> Result<Vec<u8>, Abort> {
        Err(bench_only("read"))
    }
    fn write(&mut self, _: usize, _: &[u8]) -> Result<(), Abort> {
        Err(bench_only("write"))
    }
    fn filesize(&mut self, _: usize) -> Result<u64, Abort> {
        Err(bench_only("filesize"))
    }
    fn page_cache(&mut self, path: &str, _: lua::CacheMode) -> Result<Option<String>, Abort> {
        stopping()?;
        policy::page_cache_input(&self.inputs, path)?;
        Ok(self.page_cache.get(path).cloned())
    }
}

/// The builtins' side of one arm's `bench()` in a contract test.
pub(super) struct VirtualIteration {
    pub(super) config: Arc<Value>,
    /// The build's file name, as a `binary()` result names it, or why it
    /// could not be named.
    pub(super) program: Result<String, String>,
    pub(super) calls: Calls,
    pub(super) names: Names,
    pub(super) responses: VecDeque<Response>,
    /// Every virtual file by path: the inputs the spec gave contents for,
    /// the outputs written, the spools.
    pub(super) files: BTreeMap<PathBuf, Vec<u8>>,
    pub(super) shared: Shared,
    pub(super) observed: Vec<Value>,
}

impl VirtualIteration {
    pub(super) fn new(
        config: Arc<Value>,
        program: Result<String, String>,
        inputs: &[PathBuf],
        responses: VecDeque<Response>,
        files: BTreeMap<String, Vec<u8>>,
        shared: Shared,
    ) -> Self {
        let handles = inputs
            .iter()
            .map(|path| Handle::new(path.clone(), Origin::Input))
            .collect();
        Self {
            config,
            program,
            calls: Calls::new(handles),
            names: Names::default(),
            responses,
            files: files
                .into_iter()
                .map(|(path, bytes)| (PathBuf::from(path), bytes))
                .collect(),
            shared,
            observed: Vec::new(),
        }
    }

    fn handle(&self, index: usize, function: &str) -> Result<Handle, Abort> {
        self.calls.handles.get(index, function)
    }

    /// The bytes of the file `handle` names: an input without contents in
    /// the spec is the test's fault; an output never written does not exist.
    fn contents(&mut self, handle: &Handle, function: &str) -> Result<Vec<u8>, Abort> {
        if let Some(bytes) = self.files.get(handle.path()) {
            return Ok(bytes.clone());
        }
        let shown = handle.path().display().to_string();
        if handle.origin() == Origin::Input {
            return Err(self.shared.fault(format!(
                "{function} read the input {shown}, and the spec's files gives no contents for \
                 it"
            )));
        }
        Err(Abort::Failed(format!(
            "{function}: cannot open {shown}: no such file (nothing wrote it)"
        )))
    }

    fn make(
        &mut self,
        area: NameArea,
        origin: Origin,
        name: &str,
        function: &str,
    ) -> Result<HandleRef, Abort> {
        stopping()?;
        self.names
            .claim(area, name)
            .map_err(|e| Abort::Failed(format!("{function}({name:?}): {e}")))?;
        let path = Path::new(ITERATION_ROOT).join(area.name()).join(name);
        self.calls.handles.give(Handle::new(path, origin))
    }

    fn observe(declared: &Declared) -> Value {
        let mut fields = vec![
            (
                "argv".into(),
                Value::List(declared.words.iter().cloned().map(Value::Text).collect()),
            ),
            (
                "origins".into(),
                Value::List(
                    declared
                        .word_handles
                        .iter()
                        .map(|h| {
                            Value::Text(h.as_ref().map_or("text", |h| h.origin().word()).into())
                        })
                        .collect(),
                ),
            ),
            ("env".into(), env_table(&declared.env)),
            ("reads".into(), paths(&declared.reads)),
            ("writes".into(), paths(&declared.writes)),
            ("measured".into(), Value::Bool(declared.measured)),
        ];
        let cache = |mode: crate::bench::store::CacheMode| {
            let listed: Vec<Handle> = declared
                .cache
                .iter()
                .filter(|(m, _)| *m == mode)
                .map(|(_, h)| h.clone())
                .collect();
            paths(&listed)
        };
        fields.push(("cold".into(), cache(crate::bench::store::CacheMode::Cold)));
        fields.push(("warm".into(), cache(crate::bench::store::CacheMode::Warm)));
        for (key, value) in [
            (
                "stdin",
                declared
                    .stdin
                    .as_ref()
                    .map(|h| h.path().display().to_string()),
            ),
            ("stop_at", declared.stop_at.clone()),
            ("hotpath", declared.hotpath.clone()),
            ("program", declared.program.clone()),
        ] {
            if let Some(value) = value {
                fields.push((key.into(), Value::Text(value)));
            }
        }
        Value::Table(fields)
    }

    /// The spool of call `n`'s `stream`, holding `bytes`, as a handle.
    fn spool(&mut self, stream: &str, n: usize, bytes: &[u8]) -> Result<HandleRef, Abort> {
        let path = Path::new(ITERATION_ROOT).join(stream).join(n.to_string());
        self.files.insert(path.clone(), bytes.to_vec());
        self.calls.handles.give(Handle::new(path, Origin::Spool))
    }
}

impl lua::Host for VirtualIteration {
    fn stop(&mut self) -> Result<(), Abort> {
        stopping()
    }
    fn stop_requested() -> bool {
        stop_pending()
    }
    fn begin_block(&mut self, _at: Instant) {}

    #[allow(clippy::too_many_lines)] // One call in a run's order: declare, admit, answer.
    fn binary(&mut self, words: Vec<Word>, options: CallOptions) -> Result<CallOutcome, Abort> {
        stopping()?;
        let declared = self.calls.declare(words, options)?;
        self.observed.push(Self::observe(&declared));
        let n = self.observed.len();
        // The real admission opens stdin before anything launches.
        if let Some(stdin) = &declared.stdin {
            self.contents(stdin, "binary_with(): stdin")?;
        }
        let program = match declared.program.as_deref() {
            Some(external) => short(external),
            None => match &self.program {
                Ok(name) => name.clone(),
                Err(why) => {
                    let why = format!("cannot name the build a binary() call runs: {why}");
                    return Err(self.shared.fault(why));
                }
            },
        };
        let Some(response) = self.responses.pop_front() else {
            return Err(self.shared.fault(format!(
                "bench() made call {n} ({}), and the spec's calls declares only {}",
                declared.words.join(" "),
                n - 1
            )));
        };
        let (exit, signal, stopped_at) = match &response.ending {
            Ending::Exit(code) => (Some(*code), None, None),
            Ending::Signal(signal) => (None, Some(*signal), None),
            Ending::StoppedAt(marker) => {
                if declared.stop_at.as_deref() != Some(marker.as_str()) {
                    return Err(self.shared.fault(format!(
                        "calls[{n}] stops at {marker:?}, and the call asked to stop at {}: a \
                         child ends at a marker only when its call names it in stop_at",
                        declared
                            .stop_at
                            .as_deref()
                            .map_or_else(|| "no marker".to_owned(), |m| format!("{m:?}"))
                    )));
                }
                (None, None, Some(marker.clone()))
            }
        };
        for (name, contents) in &response.outputs {
            let one_component = !name.is_empty()
                && name != "."
                && name != ".."
                && !name.contains('/')
                && !name.contains('\0');
            if !one_component {
                return Err(self.shared.fault(format!(
                    "calls[{n}].outputs names {name:?}, which is no name output() takes"
                )));
            }
            let path = Path::new(ITERATION_ROOT)
                .join(NameArea::Out.name())
                .join(name);
            let declared_written = declared
                .access
                .iter()
                .any(|(p, access)| *p == path && *access == Access::Write);
            if !declared_written {
                return Err(self.shared.fault(format!(
                    "calls[{n}] writes output {name:?}, and the call does not declare it \
                     written: pass output({name:?})'s handle, or list it in writes"
                )));
            }
            self.files.insert(path, contents.clone());
        }
        let hotpath = match (&declared.hotpath, response.hotpath) {
            (None, Some(_)) => {
                return Err(self.shared.fault(format!(
                    "calls[{n}] gives a hotpath report, and the call is not marked hotpath"
                )));
            }
            (None, None) => None,
            (Some(_), Some(report)) => Some(lua::Capture {
                status: "captured",
                report: Some(report),
                error: None,
            }),
            (Some(_), None) => Some(lua::Capture {
                status: "missing",
                report: None,
                error: None,
            }),
        };
        let (stdout, stdout_truncated) = preview(&response.stdout);
        let (stderr, stderr_truncated) = preview(&response.stderr);
        let tail = response.stderr[response.stderr.len().saturating_sub(PREVIEW_BYTES)..].to_vec();
        let stdout_file = self.spool("stdout", n - 1, &response.stdout)?;
        let stderr_file = self.spool("stderr", n - 1, &response.stderr)?;
        Ok(CallOutcome {
            program,
            exit,
            signal,
            stdout,
            stdout_truncated,
            stderr,
            stderr_truncated,
            stderr_tail: Some(tail),
            stdout_file: Some(stdout_file),
            stderr_file: Some(stderr_file),
            hotpath,
            stopped_at,
            start_error: None,
        })
    }

    fn since(&mut self, sha: &str) -> Result<bool, Abort> {
        self.shared.since(sha)
    }

    fn output(&mut self, name: &str) -> Result<HandleRef, Abort> {
        self.make(NameArea::Out, Origin::Output, name, "output")
    }

    fn scratch(&mut self, name: &str) -> Result<HandleRef, Abort> {
        self.make(NameArea::Scratch, Origin::Scratch, name, "scratch")
    }

    fn config(&mut self) -> Result<Arc<Value>, Abort> {
        stopping()?;
        Ok(Arc::clone(&self.config))
    }

    fn read(&mut self, index: usize) -> Result<Vec<u8>, Abort> {
        stopping()?;
        let handle = self.handle(index, "read()")?;
        handle
            .openable()
            .map_err(|e| Abort::Failed(format!("read(): {e}")))?;
        let bytes = self.contents(&handle, "read()")?;
        if bytes.len() > lua::MAX_STRING_BYTES {
            return Err(Abort::Failed(format!(
                "read(): {} holds more than {} bytes, the most a script's string holds",
                handle.path().display(),
                lua::MAX_STRING_BYTES
            )));
        }
        Ok(bytes)
    }

    fn write(&mut self, index: usize, contents: &[u8]) -> Result<(), Abort> {
        stopping()?;
        let handle = self.handle(index, "write()")?;
        handle
            .rewritable()
            .map_err(|e| Abort::Failed(format!("write(): {e}")))?;
        self.files
            .insert(handle.path().to_path_buf(), contents.to_vec());
        Ok(())
    }

    fn filesize(&mut self, index: usize) -> Result<u64, Abort> {
        stopping()?;
        let handle = self.handle(index, "filesize()")?;
        handle
            .openable()
            .map_err(|e| Abort::Failed(format!("filesize(): {e}")))?;
        let bytes = self.contents(&handle, "filesize()")?;
        Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
    }

    fn page_cache(&mut self, _: &str, _: lua::CacheMode) -> Result<Option<String>, Abort> {
        Err(Abort::Failed(
            "page_cache() is only available inside preflight()".into(),
        ))
    }
}
