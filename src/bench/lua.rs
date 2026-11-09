//! The seam to the linked dellingr crate, the Lua VM script benches run on.
//!
//! The crate is reached only here, by its absolute `::dellingr` path; a
//! textlint rule keeps every other file off it, and a change in the crate's
//! API touches this file alone.
//!
//! Two uses: [`check`] for `brokkr check`'s `bench` phase, which compiles a
//! script and resolves its global names, running nothing, and [`run_sample`], one iteration of a script bench in a fresh
//! State. Every builtin the script sees is marshalled here and decided by the
//! runner through [`Host`], whose types carry no dellingr value: Lua strings
//! cross as bytes, text fields are checked from those exact bytes, and a
//! builtin's failure travels back through Lua as an [`Abort`] inside a host
//! error, recovered by downcast. A pending stop reaches the script two ways:
//! every builtin but `print` asks the host for one before anything else, and
//! while a sample runs a watcher thread polls [`Host::stop_requested`] and
//! triggers the State's interrupt, so a script spinning in pure Lua fails
//! with dellingr's `Interrupted` at its next safepoint (a backward jump or a
//! call); both end the sample interrupted.
//!
//! What a table is to brokkr is never a data key, so config data and decoded
//! JSON are only data: a JSON array carries one metatable and `json.null` is
//! one table, both made at setup, protected (`__metatable` false, so a
//! script can neither read nor replace a protected metatable) and recognised
//! by identity through the State's registry; a handle carries its index in a
//! protected metatable of its own, read raw. A script can build a lookalike
//! handle on purpose; that gains nothing, since a handle resolves only to one
//! the host gave out.
//!
//! An inline script's line numbers are mapped to its config file's
//! (`super::script_lines`) when the script carries a map: compile errors,
//! every traceback frame and the location a builtin's failure is given.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ::dellingr::error::{Error, ErrorKind, StackFrame};
use ::dellingr::{ArgCount, LuaType, RetCount, RustFnHandle, State};

use super::json;
use super::script_lines::LineMap;

/// The longest string a script can hold, dellingr's limit: `read()` refuses
/// a file past it.
pub(crate) const MAX_STRING_BYTES: usize = ::dellingr::limits::MAX_STRING_BYTES;

/// Why a script did not compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompileError {
    /// 1-based line of the failure, in the config file when the script
    /// carries a line map, else in the script; 0 when unknown.
    pub(crate) line: u32,
    /// What dellingr reported, without the chunk-name and line prefix.
    pub(crate) message: String,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            f.write_str(&self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

/// What [`check`] found in a script that compiles.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct NameCheck {
    /// Each global the script reads but nothing defines, once per name at
    /// its first read, and a script that never assigns `bench`.
    pub(crate) problems: Vec<CompileError>,
    /// The line of the script's first read of `_G`, when it reads it: then
    /// its names were not checked, since a script holding the global table
    /// can define any name through it.
    pub(crate) unchecked_at: Option<u32>,
    /// Whether the script declares `validate`.
    pub(crate) validate: bool,
}

/// Compile `source` without running any of it, then resolve its global
/// names. A script's environment is closed: what a fresh State holds (the
/// builtins and dellingr's library, set up as a sample sets them up) plus
/// what the script assigns. So a bare read of any other name is nil in every
/// run, and is a typo or dead code either way. A name assigned anywhere
/// counts as defined, whether or not it is set by the time it is read. The
/// one way to define a name without assigning it is through `_G`, the only
/// door to the global table, so a script that reads `_G` is not checked.
pub(crate) fn check(source: &[u8], lines: Option<&LineMap>) -> Result<NameCheck, CompileError> {
    let script = load_script(source, None, lines.cloned())?;
    let lines = script.lines.as_deref();
    // In lexical source order, so a name's first read is its first in the
    // script.
    let accesses = script.program.globals();
    let reads: Vec<_> = accesses
        .iter()
        .filter(|a| a.kind == ::dellingr::AccessKind::Read)
        .collect();
    if let Some(global) = reads.iter().find(|a| &*a.name == b"_G") {
        return Ok(NameCheck {
            problems: Vec::new(),
            unchecked_at: Some(mapped(lines, global.line)),
            validate: script.validate,
        });
    }
    let assigned: std::collections::HashSet<&[u8]> = accesses
        .iter()
        .filter(|a| a.kind == ::dellingr::AccessKind::Write)
        .map(|a| &*a.name)
        .collect();
    let mut probe = match fresh_state::<Probe>(None) {
        Ok((state, _, _)) => state,
        Err(error) => {
            return Err(CompileError {
                line: 0,
                message: format!("cannot set up the Lua state its names resolve in: {error}"),
            });
        }
    };
    let mut seen = std::collections::HashSet::new();
    let mut problems = Vec::new();
    for read in reads {
        let name: &[u8] = &read.name;
        if assigned.contains(name) || !seen.insert(name) {
            continue;
        }
        if !provided(&mut probe, name) {
            problems.push(CompileError {
                line: mapped(lines, read.line),
                message: format!(
                    "{} is not defined: no builtin or library global has that name, and the \
                     script never assigns it",
                    String::from_utf8_lossy(name)
                ),
            });
        }
    }
    if !assigned.contains(&b"bench"[..]) {
        problems.push(CompileError {
            line: 0,
            message: "the script never assigns bench: define function bench() at its top level"
                .to_owned(),
        });
    }
    Ok(NameCheck {
        problems,
        unchecked_at: None,
        validate: script.validate,
    })
}

/// Compile a library module without running it and check its globals. A
/// module shares the State of the script that requires it, so the one rule
/// that keeps every file's name check sound on its own is that a module
/// defines no global: it exports through what it returns. So any bare
/// global assignment is a finding, and so is a read of `_G`, the one door
/// to defining a name without assigning it; every other bare read must be
/// a builtin or a library global, since nothing else in the State is the
/// module's to rely on. Returns every finding, in source order.
pub(crate) fn check_module(source: &[u8], chunk: &str) -> Result<Vec<CompileError>, CompileError> {
    let script = load_script(source, Some(chunk), None)?;
    let mut probe = match fresh_state::<Probe>(None) {
        Ok((state, _, _)) => state,
        Err(error) => {
            return Err(CompileError {
                line: 0,
                message: format!("cannot set up the Lua state its names resolve in: {error}"),
            });
        }
    };
    let mut seen = std::collections::HashSet::new();
    let mut problems = Vec::new();
    for access in script.program.globals() {
        let name: &[u8] = &access.name;
        let shown = String::from_utf8_lossy(name);
        let problem = match access.kind {
            ::dellingr::AccessKind::Write => format!(
                "assigns the global {shown}: a module defines no global, it exports through \
                 what it returns (make it local)"
            ),
            ::dellingr::AccessKind::Read if name == b"_G" => {
                "reads _G: a module defines no global, so it has no use for the global table"
                    .to_owned()
            }
            ::dellingr::AccessKind::Read if !provided(&mut probe, name) => format!(
                "{shown} is not defined: no builtin or library global has that name, and a \
                 module sees no other"
            ),
            // A read, or an access kind a later dellingr adds: neither
            // defines a name.
            _ => continue,
        };
        if seen.insert((name.to_vec(), access.kind == ::dellingr::AccessKind::Write)) {
            problems.push(CompileError {
                line: access.line,
                message: problem,
            });
        }
    }
    Ok(problems)
}

/// Whether a fresh State holds a non-nil global `name`.
fn provided(state: &mut State, name: &[u8]) -> bool {
    let Ok(name) = std::str::from_utf8(name) else {
        return false;
    };
    if state.get_global(name).is_err() {
        return false;
    }
    let held = state.typ(-1) != LuaType::Nil;
    // A failed pop leaves the probe one value deeper, which no later lookup
    // reads past: the answer stands either way.
    match state.pop(1) {
        Ok(()) | Err(_) => held,
    }
}

/// The host a name check's State is built for: it runs nothing, so no
/// builtin is ever called on it.
enum Probe {}

impl Host for Probe {
    fn stop(&mut self) -> Result<(), Abort> {
        match *self {}
    }
    fn stop_requested() -> bool {
        false
    }
    fn begin_block(&mut self, _at: Instant) {
        match *self {}
    }
    fn binary(&mut self, _: Vec<Word>, _: CallOptions) -> Result<CallOutcome, Abort> {
        match *self {}
    }
    fn since(&mut self, _: &str) -> Result<bool, Abort> {
        match *self {}
    }
    fn output(&mut self, _: &str) -> Result<HandleRef, Abort> {
        match *self {}
    }
    fn scratch(&mut self, _: &str) -> Result<HandleRef, Abort> {
        match *self {}
    }
    fn config(&mut self) -> Result<Arc<Value>, Abort> {
        match *self {}
    }
    fn read(&mut self, _: usize) -> Result<Vec<u8>, Abort> {
        match *self {}
    }
    fn write(&mut self, _: usize, _: &[u8]) -> Result<(), Abort> {
        match *self {}
    }
    fn filesize(&mut self, _: usize) -> Result<u64, Abort> {
        match *self {}
    }
}

/// A compiled script, run unchanged in every iteration of a run, with the
/// map of its lines into the config file that holds it, for an inline one.
#[derive(Clone)]
pub(crate) struct Script {
    program: ::dellingr::Program,
    lines: Option<Arc<LineMap>>,
    /// Whether the script declares a preflight: a bare global assignment of
    /// `preflight` anywhere in it, as dellingr records one while compiling.
    /// Decided here, once, from the source: never from what a run leaves.
    preflight: bool,
    /// Whether the script declares `validate`, decided as `preflight` is.
    validate: bool,
    /// The modules `require` loads, frozen with the script.
    library: Option<Arc<super::library::Library>>,
}

impl Script {
    /// Whether the script declares `preflight`, so the runner calls it for
    /// every arm before anything is built.
    pub(crate) fn has_preflight(&self) -> bool {
        self.preflight
    }

    /// Whether the script declares `validate`, so the runner calls it for
    /// every arm as soon as the params bind.
    pub(crate) fn has_validate(&self) -> bool {
        self.validate
    }

    /// The script with the library its `require` loads from.
    pub(crate) fn with_library(mut self, library: Arc<super::library::Library>) -> Self {
        self.library = Some(library);
        self
    }

    /// The chunk name the script compiled under: the one chunk its line map
    /// applies to.
    fn chunk(&self) -> Option<&[u8]> {
        self.program.source_name()
    }
}

/// Compile `source` under `chunk`, the name errors and tracebacks give it.
pub(crate) fn load_script(
    source: &[u8],
    chunk: Option<&str>,
    lines: Option<LineMap>,
) -> Result<Script, CompileError> {
    let lines = lines.map(Arc::new);
    ::dellingr::Program::compile(source, chunk.map(str::as_bytes))
        .map(|program| Script {
            preflight: assigns(&program, b"preflight"),
            validate: assigns(&program, b"validate"),
            program,
            lines: lines.clone(),
            library: None,
        })
        .map_err(|error| CompileError {
            line: mapped(lines.as_deref(), error.line_num()),
            message: error.kind().to_string(),
        })
}

/// Whether `program` makes a bare global assignment of `name` anywhere: how
/// a script declares `preflight` and `validate`.
fn assigns(program: &::dellingr::Program, name: &[u8]) -> bool {
    program
        .globals()
        .iter()
        .any(|a| a.kind == ::dellingr::AccessKind::Write && &*a.name == name)
}

/// `line` in the config file, or as it is without a map (0 stays 0).
fn mapped(lines: Option<&LineMap>, line: u32) -> u32 {
    lines.and_then(|m| m.file_line(line)).unwrap_or(line)
}

/// One argv word of a `binary()` call: text, or a handle the runner gave out
/// (`output()`, `scratch()`), by its index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Word {
    Text(String),
    Handle(usize),
}

/// A handle as the runner made it: its index and the path the script sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandleRef {
    pub(crate) index: usize,
    pub(crate) path: String,
}

/// A value crossing the seam without dellingr: the params, their keys,
/// `config()`'s table, and what `json` and `record` convert. A table's keys
/// are strings; a list is positions 1..n.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    /// JSON's null: Lua's nil at the root, `json.null` inside a table.
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    List(Vec<Value>),
    Table(Vec<(String, Value)>),
    /// A handle the runner gave out, as `output()` returns one.
    Handle(HandleRef),
}

/// What `bench()` is called with: `bench(params, keys)`, both tables.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Args {
    /// Each bound param's entry (a plain param's string).
    pub(crate) params: Vec<(String, Value)>,
    /// Each bound param's chosen key (a list for a collection).
    pub(crate) keys: Vec<(String, Value)>,
}

/// A `binary_with()` call's options, every handle by its index.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CallOptions {
    /// Added to the child's environment, sorted by name.
    pub(crate) env: Vec<(String, String)>,
    pub(crate) reads: Vec<usize>,
    pub(crate) writes: Vec<usize>,
    /// The file the child reads as its stdin.
    pub(crate) stdin: Option<usize>,
    /// `hotpath = STAGE`: capture the child's hotpath report under this
    /// stage name.
    pub(crate) hotpath: Option<String>,
    /// `stop_at = NAME_END`: end the child once that process-stream marker
    /// is seen.
    pub(crate) stop_at: Option<String>,
    /// `program = NAME`: run this external program instead of the built
    /// snapshot, a name looked up on the child's `PATH` or a path.
    pub(crate) program: Option<String>,
}

/// What one `binary()` call hands back to the script.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CallOutcome {
    /// The program's file name, for messages (`result:ok()`).
    pub(crate) program: String,
    pub(crate) exit: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stderr_truncated: bool,
    /// The child's whole stdout, its spool, as a handle.
    pub(crate) stdout_file: Option<HandleRef>,
    /// A marked call's capture, as the script sees it.
    pub(crate) hotpath: Option<Capture>,
    /// The `stop_at` marker that ended the child; then `exit` and `signal`
    /// are absent, whatever the child did after it.
    pub(crate) stopped_at: Option<String>,
    /// Why a preflight's program could not be started; then nothing ran, and
    /// `exit`, `signal` and the streams are absent.
    pub(crate) start_error: Option<StartFailure>,
}

/// A program that never started: what was being done (`resolve` or
/// `execute`), the OS error number when there was one, and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartFailure {
    pub(crate) operation: &'static str,
    pub(crate) errno: Option<i32>,
    pub(crate) message: String,
}

/// What a marked call's result carries of its capture: `hotpath_status`
/// always, `hotpath` the report as Lua values when captured, and
/// `hotpath_error` the reason when invalid or unreadable.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Capture {
    pub(crate) status: &'static str,
    pub(crate) report: Option<Value>,
    pub(crate) error: Option<String>,
}

/// Why a builtin ended the sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Abort {
    /// A stop was requested; the sample is interrupted.
    Interrupted,
    /// The sample fails with this reason (never empty): a builtin refused
    /// what the script asked of it.
    Failed(String),
    /// The script called `fail(reason)`, or `r:ok()` judged a child.
    Fail(String),
    /// brokkr's own machinery failed (an admission, a lease, a supervisor),
    /// not the script and not what it checked.
    Machinery(String),
}

impl std::fmt::Display for Abort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interrupted => f.write_str("interrupted"),
            Self::Failed(why) | Self::Fail(why) | Self::Machinery(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Abort {}

/// What the runner decides for a script's builtins. A call reaching here
/// has its arguments read and checked; whether it may run at top level is
/// decided here too (`binary`, `output`, `scratch`, `read`, `write` and
/// `filesize` raise there).
pub(crate) trait Host: Send + 'static {
    /// A pending stop, asked first by every builtin but `print`.
    fn stop(&mut self) -> Result<(), Abort>;
    /// Whether a stop is pending, asked from a watcher thread while the
    /// sample runs, so a script spinning in pure Lua is interrupted too.
    fn stop_requested() -> bool;
    /// `bench()` is about to be called: the block starts now.
    fn begin_block(&mut self, at: Instant);
    fn binary(&mut self, words: Vec<Word>, options: CallOptions) -> Result<CallOutcome, Abort>;
    fn since(&mut self, sha: &str) -> Result<bool, Abort>;
    fn output(&mut self, name: &str) -> Result<HandleRef, Abort>;
    fn scratch(&mut self, name: &str) -> Result<HandleRef, Abort>;
    /// The resolved config, for `config()`; allowed at top level.
    fn config(&mut self) -> Result<Arc<Value>, Abort>;
    /// The whole file a handle names, at most [`MAX_STRING_BYTES`].
    fn read(&mut self, handle: usize) -> Result<Vec<u8>, Abort>;
    /// Replace the file an output handle names with `contents`.
    fn write(&mut self, handle: usize, contents: &[u8]) -> Result<(), Abort>;
    fn filesize(&mut self, handle: usize) -> Result<u64, Abort>;
}

/// How one sample ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SampleOutcome {
    /// `bench()` returned nil (`None`) or a string, its verdict.
    Completed(Option<String>),
    Failed(String),
    Interrupted,
}

/// One sample: its outcome, the block's wall (absent when `bench()` was
/// never called) and `record` as extracted (absent when it never was).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sample {
    pub(crate) outcome: SampleOutcome,
    pub(crate) wall: Option<Duration>,
    pub(crate) record: Option<serde_json::Map<String, serde_json::Value>>,
    /// Every library module the sample asked `require` for, loaded or not.
    pub(crate) modules: std::collections::BTreeSet<String>,
}

/// The metatable key a handle carries its index under.
const HANDLE_MARK: &str = "__brokkr_handle";

/// The per-State context a builtin reaches through the State's user data.
struct Ctx<H> {
    host: H,
    phase: Phase,
    ok: RustFnHandle,
    marks: Marks,
    modules: Modules,
}

/// What `require` has done in this State: each module loaded, its export
/// anchored so it lives as long as the State; the chain being loaded, so a
/// cycle is refused naming it; and every name it was asked for that the
/// library holds, loaded or not, which the run records.
#[derive(Default)]
struct Modules {
    library: Option<Arc<super::library::Library>>,
    loaded: std::collections::HashMap<String, Arc<::dellingr::Anchor>>,
    loading: Vec<String>,
    attempted: std::collections::BTreeSet<String>,
}

impl Modules {
    fn of(script: &Script) -> Self {
        Self {
            library: script.library.clone(),
            ..Self::default()
        }
    }
}

/// Where the State is: what decides which builtins may run. A module a
/// script requires runs in the phase it was required from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// A chunk's top level, the script's or a module's as `require` runs it:
    /// nothing launches, no handle is made, `since()` is refused. What runs
    /// there is declarations and computation over `config()` and `json`.
    TopLevel,
    /// Inside `validate()`, as soon as the params bind: what the top level
    /// may do, and no more.
    Validate,
    /// Inside `preflight()`, before anything is built: external programs
    /// only, no handle.
    Preflight,
    /// Inside `bench()`: everything.
    Bench,
}

/// A function a script may declare beside `bench`, which the runner calls
/// for every arm before anything is built: each in a fresh State, the
/// chunk's top level first, passing by returning nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hook {
    /// `validate(params, keys)`: are these params ones the bench can run?
    /// Pure: no child, no handle, no `since()`.
    Validate,
    /// `preflight(params, keys)`: does the host provide what the bench
    /// needs? External programs only.
    Preflight,
}

impl Hook {
    /// The global the script declares it as.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::Preflight => "preflight",
        }
    }

    fn phase(self) -> Phase {
        match self {
            Self::Validate => Phase::Validate,
            Self::Preflight => Phase::Preflight,
        }
    }
}

/// How a hook ended. Every failure's reason follows the script line that
/// raised it, when one did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookOutcome {
    /// The hook returned nothing.
    Passed,
    /// The script refused: `fail(reason)` or a failed `r:ok()`, what it
    /// checked is not there.
    Refused(String),
    /// The script itself is wrong: a Lua error, a builtin it may not call or
    /// called wrongly, a hook that is not a function or returned a value.
    Broken(String),
    /// brokkr's own machinery failed while the hook ran.
    Failed(String),
    Interrupted,
}

/// What one hook did: how it ended, and every library module it asked
/// `require` for, loaded or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hooked {
    pub(crate) outcome: HookOutcome,
    pub(crate) modules: std::collections::BTreeSet<String>,
}

/// Run a declared hook in a fresh State: the chunk's top level, then
/// `hook(params, keys)`, which passes by returning nothing. Returns the host
/// back.
pub(crate) fn run_hook<H: Host>(
    hook: Hook,
    script: &Script,
    args: &Args,
    host: H,
) -> (Hooked, H) {
    let interrupt = ::dellingr::Interrupt::new();
    let (mut state, ok, marks) = match fresh_state::<H>(Some(interrupt.clone())) {
        Ok(fresh) => fresh,
        Err(error) => {
            return (
                Hooked {
                    outcome: HookOutcome::Failed(format!("cannot set up the Lua state: {error}")),
                    modules: std::collections::BTreeSet::new(),
                },
                host,
            );
        }
    };
    state.set_user_data(Some(Ctx {
        host,
        phase: Phase::TopLevel,
        ok,
        marks: marks.clone(),
        modules: Modules::of(script),
    }));
    let outcome = watched::<H, _>(&interrupt, || {
        call_hook::<H>(hook, &mut state, script, args, &marks)
    });
    let ctx = state
        .user_data_mut::<Option<Ctx<H>>>()
        .and_then(Option::take);
    match ctx {
        Some(ctx) => (
            Hooked {
                outcome,
                modules: ctx.modules.attempted,
            },
            ctx.host,
        ),
        // The context is set above and only taken here.
        None => unreachable!("the hook's host context went missing"),
    }
}

fn call_hook<H: Host>(
    hook: Hook,
    state: &mut State,
    script: &Script,
    args: &Args,
    marks: &Marks,
) -> HookOutcome {
    let name = hook.name();
    let lines = Lines::of(script);
    let broken = |error: Error| {
        if matches!(error.kind(), ErrorKind::Interrupted) {
            return HookOutcome::Interrupted;
        }
        match abort_of(&error) {
            Some(Abort::Interrupted) => HookOutcome::Interrupted,
            Some(Abort::Fail(why)) => HookOutcome::Refused(located(&error, lines, why)),
            Some(Abort::Failed(why)) => HookOutcome::Broken(located(&error, lines, why)),
            Some(Abort::Machinery(why)) => HookOutcome::Failed(located(&error, lines, why)),
            None => HookOutcome::Broken(rendered(error, lines)),
        }
    };
    if let Err(error) = state
        .load(&script.program)
        .and_then(|()| state.call(ArgCount::Fixed(0), RetCount::Fixed(0)))
    {
        return broken(error);
    }
    if let Err(error) = state.get_global(name) {
        return HookOutcome::Broken(format!("cannot read {name}: {error}"));
    }
    if state.typ(-1) != LuaType::Function {
        return HookOutcome::Broken(format!(
            "the script assigns {name}, but its top level left it a {}: define function \
             {name}(params, keys) unconditionally and branch inside it",
            state.typ(-1).as_str()
        ));
    }
    if let Err(error) = push_value(state, &Value::Table(args.params.clone()), marks) {
        return HookOutcome::Broken(format!("cannot make the params table: {error}"));
    }
    if let Err(error) = push_value(state, &Value::Table(args.keys.clone()), marks) {
        return HookOutcome::Broken(format!("cannot make the keys table: {error}"));
    }
    if let Some(ctx) = ctx::<H>(state) {
        ctx.phase = hook.phase();
    }
    // Every value it returns is counted: the function and its two arguments
    // sit at the top, and what is left above their base after the call is
    // what it returned, a `return nil` among them.
    let base = state.get_top().saturating_sub(3);
    let called = state.call(ArgCount::Fixed(2), RetCount::All);
    if let Some(ctx) = ctx::<H>(state) {
        ctx.phase = Phase::TopLevel;
    }
    if let Err(error) = called {
        return broken(error);
    }
    match state.get_top().saturating_sub(base) {
        0 => HookOutcome::Passed,
        n => HookOutcome::Broken(format!(
            "{name}() returned {n} value{} (the first a {}): it passes by returning nothing \
             and refuses with fail()",
            if n == 1 { "" } else { "s" },
            state.typ(-isize::try_from(n).unwrap_or(1)).as_str()
        )),
    }
}

/// The values brokkr recognises by identity, held in the State's registry:
/// the one protected metatable every JSON array carries, and `json.null`.
#[derive(Clone)]
struct Marks {
    array: Arc<::dellingr::Anchor>,
    null: Arc<::dellingr::Anchor>,
}

/// Run one sample of `script` in a fresh State: the chunk's top level, then
/// `bench(params, keys)` timed from its call to its return, then `record`
/// extracted. Returns the host for the next sample.
pub(crate) fn run_sample<H: Host>(script: &Script, args: &Args, host: H) -> (Sample, H) {
    let interrupt = ::dellingr::Interrupt::new();
    let (mut state, ok, marks) = match fresh_state::<H>(Some(interrupt.clone())) {
        Ok(fresh) => fresh,
        Err(error) => {
            return (
                failed(format!("cannot set up the Lua state: {error}"), None),
                host,
            );
        }
    };
    state.set_user_data(Some(Ctx {
        host,
        phase: Phase::TopLevel,
        ok,
        marks: marks.clone(),
        modules: Modules::of(script),
    }));
    let mut sample = watched::<H, _>(&interrupt, || sample::<H>(&mut state, script, args, &marks));
    let ctx = state
        .user_data_mut::<Option<Ctx<H>>>()
        .and_then(Option::take);
    match ctx {
        Some(ctx) => {
            sample.modules = ctx.modules.attempted;
            (sample, ctx.host)
        }
        // The context is set above and only taken here.
        None => unreachable!("the sample's host context went missing"),
    }
}

/// A State as every sample starts in, its builtins installed: the one setup
/// a sample and a name check share, so they resolve the same names.
fn fresh_state<H: Host>(
    interrupt: Option<::dellingr::Interrupt>,
) -> Result<(State, RustFnHandle, Marks), Error> {
    let mut builder = State::builder().callbacks(Box::new(Console));
    if let Some(interrupt) = interrupt {
        builder = builder.interrupt(interrupt);
    }
    let mut state = builder.build();
    let (ok, marks) = install::<H>(&mut state)?;
    Ok((state, ok, marks))
}

/// How often the watcher asks the host for a pending stop.
const STOP_POLL: Duration = Duration::from_millis(10);

/// Run `work` while a watcher thread triggers `interrupt` once the host
/// reports a pending stop. The watcher ends with `work`.
fn watched<H: Host, T>(interrupt: &::dellingr::Interrupt, work: impl FnOnce() -> T) -> T {
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                if H::stop_requested() {
                    interrupt.trigger();
                    return;
                }
                std::thread::park_timeout(STOP_POLL);
            }
        });
        let result = work();
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        watcher.thread().unpark();
        result
    })
}

fn failed(reason: String, wall: Option<Duration>) -> Sample {
    Sample {
        outcome: SampleOutcome::Failed(reason),
        wall,
        record: None,
        modules: std::collections::BTreeSet::new(),
    }
}

fn sample<H: Host>(state: &mut State, script: &Script, args: &Args, marks: &Marks) -> Sample {
    let lines = Lines::of(script);
    if let Err(error) = state
        .load(&script.program)
        .and_then(|()| state.call(ArgCount::Fixed(0), RetCount::Fixed(0)))
    {
        return ended(error, lines, None);
    }
    if let Err(error) = state.get_global("bench") {
        return failed(format!("cannot read bench: {error}"), None);
    }
    if state.typ(-1) != LuaType::Function {
        return failed(
            "the script defines no bench() function at its top level".to_owned(),
            None,
        );
    }
    if let Err(error) = push_value(state, &Value::Table(args.params.clone()), marks) {
        return failed(format!("cannot make the params table: {error}"), None);
    }
    if let Err(error) = push_value(state, &Value::Table(args.keys.clone()), marks) {
        return failed(format!("cannot make the keys table: {error}"), None);
    }
    let start = Instant::now();
    if let Some(ctx) = ctx::<H>(state) {
        ctx.phase = Phase::Bench;
        ctx.host.begin_block(start);
    }
    let called = state.call(ArgCount::Fixed(2), RetCount::Fixed(1));
    let wall = start.elapsed();
    if let Some(ctx) = ctx::<H>(state) {
        ctx.phase = Phase::TopLevel;
    }
    if let Err(error) = called {
        return ended(error, lines, Some(wall));
    }
    let verdict = match state.typ(-1) {
        LuaType::Nil => None,
        LuaType::String => match state.to_bytes(-1).map(|b| String::from_utf8(b.to_vec())) {
            Ok(Ok(text)) => Some(text),
            _ => {
                return failed(
                    "bench() returned a verdict that is not UTF-8".to_owned(),
                    Some(wall),
                );
            }
        },
        other => {
            return failed(
                format!(
                    "bench() returned a {}: a verdict is nil or a string",
                    other.as_str()
                ),
                Some(wall),
            );
        }
    };
    match extract_record(state, marks) {
        Ok(record) => Sample {
            outcome: SampleOutcome::Completed(verdict),
            wall: Some(wall),
            record: Some(record),
            modules: std::collections::BTreeSet::new(),
        },
        Err(why) => failed(format!("record {why}"), Some(wall)),
    }
}

/// A script's line map and the one chunk it numbers: a module's frames,
/// and any other chunk's, keep their own lines.
#[derive(Clone, Copy)]
struct Lines<'a> {
    map: Option<&'a LineMap>,
    chunk: Option<&'a [u8]>,
}

impl<'a> Lines<'a> {
    fn of(script: &'a Script) -> Self {
        Self {
            map: script.lines.as_deref(),
            chunk: script.chunk(),
        }
    }

    /// `line` of `source` as the reader should see it.
    fn at(self, source: Option<&[u8]>, line: u32) -> u32 {
        if source.is_some() && source == self.chunk {
            mapped(self.map, line)
        } else {
            line
        }
    }
}

/// A sample ended by an error out of the chunk or `bench()`. A builtin's
/// failure is its reason after the script line that called it; any other
/// error is dellingr's own rendering, its traceback included, with every
/// line of the script's own chunk mapped.
fn ended(error: Error, lines: Lines<'_>, wall: Option<Duration>) -> Sample {
    let outcome = match abort_of(&error) {
        _ if matches!(error.kind(), ErrorKind::Interrupted) => SampleOutcome::Interrupted,
        Some(Abort::Interrupted) => SampleOutcome::Interrupted,
        Some(Abort::Failed(why) | Abort::Fail(why) | Abort::Machinery(why)) => {
            SampleOutcome::Failed(located(&error, lines, why))
        }
        None => {
            let shown = rendered(error, lines);
            SampleOutcome::Failed(if shown.trim().is_empty() {
                "the script failed".to_owned()
            } else {
                shown
            })
        }
    };
    Sample {
        outcome,
        wall,
        record: None,
        modules: std::collections::BTreeSet::new(),
    }
}

/// `why` after `chunk:line:` of the innermost script frame, or alone when no
/// frame knows its line.
fn located(error: &Error, lines: Lines<'_>, why: &str) -> String {
    let frame = error
        .stack_trace()
        .iter()
        .find(|f| f.source.is_some() && f.line != 0);
    match frame.and_then(|f| f.source.as_deref().map(|s| (s, f.line))) {
        Some((source, line)) => format!(
            "{}:{}: {why}",
            String::from_utf8_lossy(source),
            lines.at(Some(source), line)
        ),
        None => why.to_owned(),
    }
}

/// dellingr's rendering of `error` with every line of the script's own chunk
/// mapped.
fn rendered(error: Error, lines: Lines<'_>) -> String {
    if lines.map.is_none() {
        return error.to_string();
    }
    if error.stack_trace().is_empty() {
        // Rendered from the error's own position, which no frame carries.
        let at = lines.at(error.source_name(), error.line_num());
        return match error.source_name() {
            Some(source) if at != 0 => {
                format!("{}:{at}: {}", String::from_utf8_lossy(source), error.kind())
            }
            _ => error.kind().to_string(),
        };
    }
    let frames: Vec<StackFrame> = error
        .stack_trace()
        .iter()
        .map(|frame| StackFrame {
            line: lines.at(frame.source.as_deref(), frame.line),
            defined_line: lines.at(frame.source.as_deref(), frame.defined_line),
            ..frame.clone()
        })
        .collect();
    error.with_stack_trace(frames).to_string()
}

fn abort_of(error: &Error) -> Option<&Abort> {
    match error.kind() {
        ErrorKind::HostError(inner) => inner.downcast_ref::<Abort>(),
        _ => None,
    }
}

/// The builtins, as globals (so dellingr names them in errors), the `json`
/// table and the `record` table. Returns the handle of `result:ok` and the
/// anchored marks.
fn install<H: Host>(state: &mut State) -> Result<(RustFnHandle, Marks), Error> {
    let builtins: [(&str, ::dellingr::RustFunc); 11] = [
        ("require", lua_require::<H>),
        ("binary", lua_binary::<H>),
        ("binary_with", lua_binary_with::<H>),
        ("since", lua_since::<H>),
        ("fail", lua_fail::<H>),
        ("output", lua_output::<H>),
        ("scratch", lua_scratch::<H>),
        ("config", lua_config::<H>),
        ("read", lua_read::<H>),
        ("write", lua_write::<H>),
        ("filesize", lua_filesize::<H>),
    ];
    for (name, function) in builtins {
        let handle = state.add_rust_fn(name, function)?;
        state.push_rust_fn(handle)?;
        state.set_global(name)?;
    }
    state.new_table()?;
    let table = top(state);
    let functions: [(&str, ::dellingr::RustFunc); 2] = [
        ("encode", lua_json_encode::<H>),
        ("decode", lua_json_decode::<H>),
    ];
    for (name, function) in functions {
        let handle = state.add_rust_fn(name, function)?;
        state.push_string(name)?;
        state.push_rust_fn(handle)?;
        state.set_table_raw(table)?;
    }
    // The array metatable and json.null: each a table only brokkr holds a
    // reference to, behind a protected metatable a script cannot read or
    // replace.
    protected_metatable(state)?;
    let array = Arc::new(state.anchor()?);
    state.push_string("null")?;
    state.new_table()?;
    let null = top(state);
    protected_metatable(state)?;
    state.set_metatable_of(null)?;
    let null = Arc::new(state.anchor_at(null)?);
    state.set_table_raw(table)?;
    state.set_global("json")?;
    state.new_table()?;
    state.set_global("record")?;
    let ok = state.add_rust_fn("ok", lua_result_ok::<H>)?;
    Ok((ok, Marks { array, null }))
}

/// Push a fresh empty metatable with `__metatable` false: `getmetatable`
/// answers false and `setmetatable` refuses on whatever carries it.
fn protected_metatable(state: &mut State) -> Result<(), Error> {
    state.new_table()?;
    let meta = top(state);
    state.push_string("__metatable")?;
    state.push_boolean(false)?;
    state.set_table_raw(meta)
}

fn ctx<H: Host>(state: &mut State) -> Option<&mut Ctx<H>> {
    state
        .user_data_mut::<Option<Ctx<H>>>()
        .and_then(Option::as_mut)
}

/// Raise `abort` through Lua.
fn raise(state: &State, abort: Abort) -> Error {
    state.error(ErrorKind::HostError(Box::new(abort)))
}

fn raise_failed(state: &State, why: String) -> Error {
    raise(state, Abort::Failed(why))
}

/// Every builtin's first step but `print`'s: a pending stop ends the sample
/// here, the one place a script spinning in Lua meets it.
fn stop_first<H: Host>(state: &mut State) -> Result<(), Error> {
    let stopped = match ctx::<H>(state) {
        Some(ctx) => ctx.host.stop(),
        None => Err(Abort::Failed("no bench context".into())),
    };
    stopped.map_err(|abort| raise(state, abort))
}

/// The host, for a builtin only `bench()` may call.
fn bench_host<'a, H: Host>(state: &'a mut State, name: &str) -> Result<&'a mut H, Error> {
    let phase = ctx::<H>(state).map(|c| c.phase);
    if phase != Some(Phase::Bench) {
        let place = phase.map_or("at a chunk's top level", Phase::place);
        return Err(raise_failed(
            state,
            format!("{name}() is only available inside bench(), not {place}"),
        ));
    }
    match ctx::<H>(state) {
        Some(ctx) => Ok(&mut ctx.host),
        None => Err(raise_failed(state, format!("{name}(): no bench context"))),
    }
}

/// The host, for a call: inside `bench()` any, inside `preflight()` only an
/// external program's, since the build does not exist yet.
fn launch_host<'a, H: Host>(
    state: &'a mut State,
    name: &str,
    external: bool,
) -> Result<&'a mut H, Error> {
    let phase = ctx::<H>(state).map(|c| c.phase);
    match phase {
        Some(Phase::Bench) => {}
        Some(Phase::Preflight) if external => {}
        Some(Phase::Preflight) => {
            return Err(raise_failed(
                state,
                format!(
                    "{name}() runs the build, which does not exist yet inside preflight(): run \
                     an external program with binary_with({{program = ...}})"
                ),
            ));
        }
        other => {
            let place = other.map_or("at a chunk's top level", Phase::place);
            return Err(raise_failed(
                state,
                format!("{name}() is only available inside bench() and preflight(), not {place}"),
            ));
        }
    }
    match ctx::<H>(state) {
        Some(ctx) => Ok(&mut ctx.host),
        None => Err(raise_failed(state, format!("{name}(): no bench context"))),
    }
}

/// The host, for `since()`: a git question, asked inside `bench()` and
/// `preflight()` only, so a chunk's top level and `validate()` stay pure.
fn git_host<'a, H: Host>(state: &'a mut State, name: &str) -> Result<&'a mut H, Error> {
    let phase = ctx::<H>(state).map(|c| c.phase);
    if !matches!(phase, Some(Phase::Bench | Phase::Preflight)) {
        let place = phase.map_or("at a chunk's top level", Phase::place);
        return Err(raise_failed(
            state,
            format!("{name}() is only available inside bench() and preflight(), not {place}"),
        ));
    }
    match ctx::<H>(state) {
        Some(ctx) => Ok(&mut ctx.host),
        None => Err(raise_failed(state, format!("{name}(): no bench context"))),
    }
}

impl Phase {
    /// Where a refused builtin was called, for its message.
    fn place(self) -> &'static str {
        match self {
            Self::TopLevel => "at a chunk's top level (the script's, or a module's as require runs it)",
            Self::Validate => "inside validate(), which only judges the params",
            Self::Preflight => "inside preflight(), before anything is built",
            Self::Bench => "inside bench()",
        }
    }
}

/// The host, for a builtin allowed at top level too.
fn any_host<'a, H: Host>(state: &'a mut State, name: &str) -> Result<&'a mut H, Error> {
    if ctx::<H>(state).is_none() {
        return Err(raise_failed(state, format!("{name}(): no bench context")));
    }
    match ctx::<H>(state) {
        Some(ctx) => Ok(&mut ctx.host),
        None => Err(raise_failed(state, format!("{name}(): no bench context"))),
    }
}

fn top(state: &State) -> isize {
    isize::try_from(state.get_top()).unwrap_or(isize::MAX)
}

/// The exact bytes of the string at `idx` as UTF-8 text.
fn text_at(state: &State, idx: isize, what: &str) -> Result<String, Error> {
    let bytes = state.to_bytes(idx)?.to_vec();
    String::from_utf8(bytes).map_err(|_| raise_failed(state, format!("{what} is not UTF-8")))
}

/// A string argument (the name of `output`, `scratch`, the sha of `since`).
fn string_arg(state: &mut State, idx: isize, function: &str) -> Result<String, Error> {
    if state.typ(idx) != LuaType::String {
        return Err(raise_failed(
            state,
            format!(
                "{function}()'s argument {idx} is a {}: expected a string",
                state.typ(idx).as_str()
            ),
        ));
    }
    text_at(state, idx, &format!("{function}()'s argument"))
}

fn lua_since<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    let sha = string_arg(state, 1, "since")?;
    let answer = git_host::<H>(state, "since")?.since(&sha);
    match answer {
        Ok(yes) => {
            state.push_boolean(yes)?;
            Ok(1)
        }
        Err(abort) => Err(raise(state, abort)),
    }
}

/// `require(name)`: the export of the library module `name`, loaded on its
/// first `require` in this State, its chunk at top level, and cached
/// for every later one. A module returns exactly one non-nil value; a cycle
/// is refused naming its chain; a module's own error comes back unchanged.
fn lua_require<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    let name = string_arg(state, 1, "require")?;
    if !super::library::is_module_name(&name) {
        return Err(raise_failed(
            state,
            format!(
                "require({name:?}): a module name is dot-separated segments of letters, digits \
                 and _, the module's path under bench_lib without .lua"
            ),
        ));
    }
    let program = match find_module::<H>(state, &name) {
        Ok(Found::Loaded(export)) => {
            state.push_anchor(&export)?;
            return Ok(1);
        }
        Ok(Found::Load(program)) => program,
        Err(why) => return Err(raise_failed(state, why)),
    };
    // A module's chunk runs at top level whatever phase asked for it, so its
    // initialization is the same in every phase; its exported functions run
    // in the phase that later calls them.
    let caller = ctx::<H>(state).map(|c| std::mem::replace(&mut c.phase, Phase::TopLevel));
    let base = state.get_top();
    let ran = state
        .load(&program)
        .and_then(|()| state.call(ArgCount::Fixed(0), RetCount::All));
    if let Some(ctx) = ctx::<H>(state) {
        ctx.modules.loading.pop();
        if let Some(caller) = caller {
            ctx.phase = caller;
        }
    }
    ran?;
    let returned = state.get_top().saturating_sub(base);
    if returned != 1 || state.typ(-1) == LuaType::Nil {
        return Err(raise_failed(
            state,
            format!(
                "require({name:?}): the module returned {returned} value{}: a module returns \
                 exactly one non-nil value, its export",
                if returned == 1 { " (nil)" } else { "s" }
            ),
        ));
    }
    let export = Arc::new(state.anchor()?);
    state.push_anchor(&export)?;
    if let Some(ctx) = ctx::<H>(state) {
        ctx.modules.loaded.insert(name, export);
    }
    Ok(1)
}

/// What `require(name)` finds in this State's context.
enum Found {
    /// Loaded before: its export.
    Loaded(Arc<::dellingr::Anchor>),
    /// To load now: its compiled chunk, the name recorded as attempted and
    /// pushed on the loading chain.
    Load(::dellingr::Program),
}

fn find_module<H: Host>(state: &mut State, name: &str) -> Result<Found, String> {
    let Some(ctx) = ctx::<H>(state) else {
        return Err("require(): no bench context".into());
    };
    let modules = &mut ctx.modules;
    if let Some(export) = modules.loaded.get(name) {
        return Ok(Found::Loaded(Arc::clone(export)));
    }
    if modules.loading.iter().any(|loading| loading == name) {
        return Err(format!(
            "require({name:?}): a cycle, {} -> {name}",
            modules.loading.join(" -> ")
        ));
    }
    let Some(library) = modules.library.clone() else {
        return Err(format!(
            "require({name:?}): no bench_lib is configured: set bench_lib = \"dir\" in \
             brokkr.toml to the directory the modules live in"
        ));
    };
    let Some(module) = library.modules.get(name) else {
        return Err(format!(
            "require({name:?}): bench_lib {} holds no module {name}",
            library.dir.display()
        ));
    };
    modules.attempted.insert(name.to_owned());
    match &module.compiled {
        Ok(script) => {
            modules.loading.push(name.to_owned());
            Ok(Found::Load(script.program.clone()))
        }
        Err(error) => Err(format!(
            "require({name:?}): {} does not compile: {error}",
            module.path.display()
        )),
    }
}

fn lua_fail<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    let reason = if top(state) >= 1 && state.typ(1) == LuaType::String {
        text_at(state, 1, "fail()'s reason")?
    } else if top(state) >= 1 && state.typ(1) == LuaType::Number {
        String::from_utf8_lossy(&state.tostring_bytes(1)?).into_owned()
    } else {
        String::new()
    };
    let reason = if reason.trim().is_empty() {
        "fail() called without a reason".to_owned()
    } else {
        reason
    };
    Err(raise(state, Abort::Fail(reason)))
}

fn lua_output<H: Host>(state: &mut State) -> Result<u8, Error> {
    make_handle::<H>(state, "output", H::output)
}

fn lua_scratch<H: Host>(state: &mut State) -> Result<u8, Error> {
    make_handle::<H>(state, "scratch", H::scratch)
}

fn make_handle<H: Host>(
    state: &mut State,
    function: &str,
    make: impl FnOnce(&mut H, &str) -> Result<HandleRef, Abort>,
) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    bench_host::<H>(state, function)?;
    let name = string_arg(state, 1, function)?;
    let made = make(bench_host::<H>(state, function)?, &name);
    let handle = made.map_err(|abort| raise(state, abort))?;
    push_handle(state, &handle)?;
    Ok(1)
}

/// A handle's table: `path`, and its index in a protected metatable of its
/// own, under [`HANDLE_MARK`].
fn push_handle(state: &mut State, handle: &HandleRef) -> Result<(), Error> {
    state.new_table()?;
    let table = top(state);
    state.push_string("path")?;
    state.push_string(&handle.path)?;
    state.set_table_raw(table)?;
    protected_metatable(state)?;
    let meta = top(state);
    state.push_string(HANDLE_MARK)?;
    state.push_number(f64::from(u32::try_from(handle.index).unwrap_or(u32::MAX)))?;
    state.set_table_raw(meta)?;
    state.set_metatable_of(table)
}

/// The marks, for a builtin.
fn marks<H: Host>(state: &mut State) -> Result<Marks, Error> {
    match ctx::<H>(state) {
        Some(ctx) => Ok(ctx.marks.clone()),
        None => Err(raise_failed(state, "no bench context".into())),
    }
}

/// Push `value` as a fresh Lua value. A list carries the JSON array
/// metatable, so an empty one stays a list; a null is `json.null`.
fn push_value(state: &mut State, value: &Value, marks: &Marks) -> Result<(), Error> {
    match value {
        Value::Null => state.push_anchor(&marks.null),
        Value::Bool(b) => state.push_boolean(*b),
        Value::Number(n) => state.push_number(*n),
        Value::Text(text) => state.push_string(text),
        Value::Handle(handle) => push_handle(state, handle),
        Value::List(items) => {
            state.new_table()?;
            let table = top(state);
            for (i, item) in items.iter().enumerate() {
                #[allow(clippy::cast_precision_loss)] // A list is far shorter than 2^53.
                state.push_number((i + 1) as f64)?;
                push_value(state, item, marks)?;
                state.set_table_raw(table)?;
            }
            state.push_anchor(&marks.array)?;
            state.set_metatable_of(table)
        }
        Value::Table(fields) => {
            state.new_table()?;
            let table = top(state);
            for (key, item) in fields {
                state.push_string(key)?;
                push_value(state, item, marks)?;
                state.set_table_raw(table)?;
            }
            Ok(())
        }
    }
}

/// `config()`: a fresh copy of the resolved config each call.
fn lua_config<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    let view = any_host::<H>(state, "config")?.config();
    let view = view.map_err(|abort| raise(state, abort))?;
    let marks = marks::<H>(state)?;
    push_value(state, &view, &marks)?;
    Ok(1)
}

/// What a table is to brokkr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Plain,
    Handle(usize),
    Array,
    Null,
}

/// What the value at `idx` (absolute) is, from its real metatable read raw:
/// a handle by the index its own metatable holds, an array by carrying the
/// anchored array metatable, a null by being the anchored `json.null`. Only
/// a table is ever anything but plain.
fn marker(state: &mut State, idx: isize, marks: Option<&Marks>) -> Result<Marker, Error> {
    if state.typ(idx) != LuaType::Table {
        return Ok(Marker::Plain);
    }
    if let Some(marks) = marks {
        state.push_anchor(&marks.null)?;
        let null = state.raw_equal(idx, -1);
        state.pop(1)?;
        if null {
            return Ok(Marker::Null);
        }
    }
    state.get_metatable_of(idx)?;
    if state.typ(-1) != LuaType::Table {
        state.pop(1)?;
        return Ok(Marker::Plain);
    }
    let meta = top(state);
    if let Some(marks) = marks {
        state.push_anchor(&marks.array)?;
        let array = state.raw_equal(meta, -1);
        state.pop(1)?;
        if array {
            state.pop(1)?;
            return Ok(Marker::Array);
        }
    }
    state.push_string(HANDLE_MARK)?;
    state.get_table_raw(meta)?;
    let handle = match state.typ(-1) {
        LuaType::Nil => None,
        LuaType::Number => Some(exact_index(state.to_number(-1)?)),
        _ => Some(None),
    };
    state.pop(2)?;
    match handle {
        None => Ok(Marker::Plain),
        Some(Some(index)) => Ok(Marker::Handle(index)),
        Some(None) => Err(raise_failed(
            state,
            "a table whose metatable carries a malformed handle index".into(),
        )),
    }
}

/// An exact non-negative integer below 2^32 as an index.
fn exact_index(n: f64) -> Option<usize> {
    (n >= 0.0 && n.fract() == 0.0 && n < 4_294_967_296.0).then(|| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = n as usize;
        n
    })
}

/// The handle index the value at `idx` carries, if it is a handle.
fn handle_index(state: &mut State, idx: isize) -> Result<Option<usize>, Error> {
    Ok(match marker(state, idx, None)? {
        Marker::Handle(index) => Some(index),
        _ => None,
    })
}

/// A handle argument of a file builtin.
fn handle_arg(state: &mut State, idx: isize, function: &str) -> Result<usize, Error> {
    match handle_index(state, idx)? {
        Some(index) => Ok(index),
        None => Err(raise_failed(
            state,
            format!(
                "{function}()'s argument {idx} is a {}: expected a handle (a bound file, \
                 output(), or a result's stdout_file)",
                state.typ(idx).as_str()
            ),
        )),
    }
}

/// One argv word from a string, number or handle at `idx`.
fn word_at(state: &mut State, idx: isize, what: &str) -> Result<Word, Error> {
    match state.typ(idx) {
        LuaType::String => Ok(Word::Text(text_at(state, idx, what)?)),
        LuaType::Number => Ok(Word::Text(number_text(state, idx)?)),
        LuaType::Table => match handle_index(state, idx)? {
            Some(index) => Ok(Word::Handle(index)),
            None => Err(raise_failed(
                state,
                format!("{what} is a table that is not a handle"),
            )),
        },
        other => Err(raise_failed(
            state,
            format!(
                "{what} is a {}: expected a string, number or handle",
                other.as_str()
            ),
        )),
    }
}

/// A number as Lua writes it, the spelling argv and env values take.
fn number_text(state: &mut State, idx: isize) -> Result<String, Error> {
    let rendered = state.tostring_bytes(idx)?;
    Ok(String::from_utf8_lossy(&rendered).into_owned())
}

/// The value of one flag of a flag table.
enum Flag {
    Bare,
    Dropped,
    One(Word),
    Many(Vec<Word>),
}

/// A flag table's flags, sorted by key, appended to `words`: `-k` for a
/// one-character key, `--key` otherwise; `true` bare, `false` dropped, a
/// string, number or handle its own following word, a list repeating the
/// flag once per element.
fn flags_at(state: &mut State, table: isize, words: &mut Vec<Word>) -> Result<(), Error> {
    let mut flags: Vec<(String, Flag)> = Vec::new();
    state.push_nil()?;
    while state.table_next(table)? {
        let key_idx = top(state) - 1;
        let value_idx = top(state);
        if state.typ(key_idx) != LuaType::String {
            return Err(raise_failed(
                state,
                "binary(): a flag table's keys are flag names (strings)".into(),
            ));
        }
        let key = text_at(state, key_idx, "a flag name")?;
        if key.is_empty() {
            return Err(raise_failed(state, "binary(): an empty flag name".into()));
        }
        let what = format!("binary(): flag {key}");
        let flag = match state.typ(value_idx) {
            LuaType::Boolean if state.to_boolean(value_idx) => Flag::Bare,
            LuaType::Boolean => Flag::Dropped,
            LuaType::Table if handle_index(state, value_idx)?.is_none() => {
                Flag::Many(list_at(state, value_idx, &what, word_at)?)
            }
            _ => Flag::One(word_at(state, value_idx, &what)?),
        };
        flags.push((key, flag));
        state.pop(1)?;
    }
    // The finished iteration leaves its nil key behind.
    state.pop(1)?;
    flags.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, flag) in flags {
        let spelled = if key.chars().count() == 1 {
            format!("-{key}")
        } else {
            format!("--{key}")
        };
        match flag {
            Flag::Dropped => {}
            Flag::Bare => words.push(Word::Text(spelled)),
            Flag::One(value) => {
                words.push(Word::Text(spelled));
                words.push(value);
            }
            Flag::Many(values) => {
                for value in values {
                    words.push(Word::Text(spelled.clone()));
                    words.push(value);
                }
            }
        }
    }
    Ok(())
}

/// A list's items: exactly the positions 1..n, each read by `item`, in
/// order. A named key or a gap is refused rather than dropped.
fn list_at<T>(
    state: &mut State,
    table: isize,
    what: &str,
    item: fn(&mut State, isize, &str) -> Result<T, Error>,
) -> Result<Vec<T>, Error> {
    let mut items: Vec<(usize, T)> = Vec::new();
    state.push_nil()?;
    while state.table_next(table)? {
        let key_idx = top(state) - 1;
        let value_idx = top(state);
        let position = (state.typ(key_idx) == LuaType::Number)
            .then(|| state.to_number(key_idx))
            .transpose()?
            .filter(|n| *n >= 1.0)
            .and_then(exact_index);
        let Some(position) = position else {
            return Err(raise_failed(
                state,
                format!("{what} is a table that is neither a handle nor a list 1..n"),
            ));
        };
        let read = item(state, value_idx, &format!("{what}[{position}]"))?;
        items.push((position, read));
        state.pop(1)?;
    }
    // The finished iteration leaves its nil key behind.
    state.pop(1)?;
    items.sort_by_key(|(position, _)| *position);
    if items.iter().enumerate().any(|(i, (p, _))| *p != i + 1) {
        return Err(raise_failed(state, format!("{what} is a list with gaps")));
    }
    Ok(items.into_iter().map(|(_, item)| item).collect())
}

/// `binary()`'s argv words from the arguments at `first` and after.
fn words_from(state: &mut State, first: isize) -> Result<Vec<Word>, Error> {
    let count = top(state);
    let mut words = Vec::new();
    for idx in first..=count {
        let what = format!("binary()'s argument {}", idx - first + 1);
        if state.typ(idx) == LuaType::Table && handle_index(state, idx)?.is_none() {
            flags_at(state, idx, &mut words)?;
        } else {
            words.push(word_at(state, idx, &what)?);
        }
    }
    Ok(words)
}

fn lua_binary<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    launch_host::<H>(state, "binary", false)?;
    let words = words_from(state, 1)?;
    call_binary::<H>(state, "binary", words, CallOptions::default())
}

fn lua_binary_with<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    launch_host::<H>(state, "binary_with", true)?;
    let options = options_at(state, 1)?;
    launch_host::<H>(state, "binary_with", options.program.is_some())?;
    let words = words_from(state, 2)?;
    call_binary::<H>(state, "binary_with", words, options)
}

/// `binary_with()`'s first argument: a plain table of `env`, `reads`,
/// `writes` and `stdin`, every other key refused.
fn options_at(state: &mut State, idx: isize) -> Result<CallOptions, Error> {
    if state.typ(idx) != LuaType::Table || handle_index(state, idx)?.is_some() {
        return Err(raise_failed(
            state,
            format!(
                "binary_with()'s first argument is a {}: expected a table of options \
                 (env, reads, writes, stdin, hotpath, stop_at, program)",
                state.typ(idx).as_str()
            ),
        ));
    }
    let mut options = CallOptions::default();
    state.push_nil()?;
    while state.table_next(idx)? {
        let key_idx = top(state) - 1;
        let value_idx = top(state);
        let key = if state.typ(key_idx) == LuaType::String {
            text_at(state, key_idx, "an option name")?
        } else {
            String::new()
        };
        match key.as_str() {
            "env" => options.env = env_at(state, value_idx)?,
            "reads" | "writes" => {
                let what = format!("binary_with(): {key}");
                if state.typ(value_idx) != LuaType::Table
                    || handle_index(state, value_idx)?.is_some()
                {
                    return Err(raise_failed(state, format!("{what} is a list of handles")));
                }
                let handles = list_at(state, value_idx, &what, listed_handle)?;
                if key == "reads" {
                    options.reads = handles;
                } else {
                    options.writes = handles;
                }
            }
            "stdin" => {
                options.stdin = Some(listed_handle(state, value_idx, "binary_with(): stdin")?);
            }
            "hotpath" => options.hotpath = Some(stage_at(state, value_idx)?),
            "stop_at" => options.stop_at = Some(stop_marker_at(state, value_idx)?),
            "program" => options.program = Some(program_at(state, value_idx)?),
            _ => {
                let shown = if key.is_empty() {
                    format!("a {} key", state.typ(key_idx).as_str())
                } else {
                    format!("{key:?}")
                };
                return Err(raise_failed(
                    state,
                    format!(
                        "binary_with(): unknown option {shown}: options are env, reads, writes, \
                         stdin, hotpath, stop_at and program"
                    ),
                ));
            }
        }
        state.pop(1)?;
    }
    state.pop(1)?;
    if options.hotpath.is_some()
        && let Some((name, _)) = options.env.iter().find(|(n, _)| n.starts_with("HOTPATH_"))
    {
        let name = name.clone();
        return Err(raise_failed(
            state,
            format!(
                "binary_with(): env name {name} is brokkr's beside hotpath: brokkr sets \
                 HOTPATH_* to capture the report"
            ),
        ));
    }
    Ok(options)
}

/// `hotpath`: a stage name, a param name's grammar.
fn stage_at(state: &mut State, idx: isize) -> Result<String, Error> {
    if state.typ(idx) != LuaType::String {
        return Err(raise_failed(
            state,
            format!(
                "binary_with(): hotpath is a {}: expected a stage name",
                state.typ(idx).as_str()
            ),
        ));
    }
    let stage = text_at(state, idx, "binary_with(): hotpath")?;
    if !crate::config::bench::is_param_name(&stage) {
        return Err(raise_failed(
            state,
            format!(
                "binary_with(): hotpath stage {stage:?} is not a name: lowercase, starting with \
                 a letter, then letters, digits, '_' or '-'"
            ),
        ));
    }
    Ok(stage)
}

/// `stop_at`: an exact span-end marker name, `FOO_END`. The lenient `FOO`
/// and the `-FOO` shorthand `--stop` takes are refused: a bare name matches
/// two markers, and which one ends the child would depend on how the FIFO
/// was drained.
fn stop_marker_at(state: &mut State, idx: isize) -> Result<String, Error> {
    let what = "binary_with(): stop_at";
    if state.typ(idx) != LuaType::String {
        return Err(raise_failed(
            state,
            format!(
                "{what} is a {}: expected a marker name",
                state.typ(idx).as_str()
            ),
        ));
    }
    let marker = text_at(state, idx, what)?;
    // Letters, digits and `_` only: a leading `@` or `%` is the FIFO's
    // counter or call-tag syntax, never a process-stream marker.
    let exact = marker.len() > "_END".len()
        && marker.ends_with("_END")
        && marker
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !exact {
        return Err(raise_failed(
            state,
            format!(
                "{what} {marker:?} is not an exact span-end marker: name the one that ends the \
                 phase, as FOO_END"
            ),
        ));
    }
    Ok(marker)
}

/// `program`: a program name or a path, a non-empty string without NUL.
/// Where it resolves is the host's to decide.
fn program_at(state: &mut State, idx: isize) -> Result<String, Error> {
    let what = "binary_with(): program";
    if state.typ(idx) != LuaType::String {
        return Err(raise_failed(
            state,
            format!(
                "{what} is a {}: expected a program name or a path",
                state.typ(idx).as_str()
            ),
        ));
    }
    let program = text_at(state, idx, what)?;
    if program.is_empty() || program.contains('\0') {
        return Err(raise_failed(
            state,
            format!("{what} {program:?} is empty or holds a NUL byte"),
        ));
    }
    Ok(program)
}

/// A handle in an option, refused when it is anything else (a path string
/// declares nothing).
fn listed_handle(state: &mut State, idx: isize, what: &str) -> Result<usize, Error> {
    match handle_index(state, idx)? {
        Some(index) => Ok(index),
        None => Err(raise_failed(
            state,
            format!(
                "{what} is a {}: expected a handle (a path string declares nothing)",
                state.typ(idx).as_str()
            ),
        )),
    }
}

/// `env`: names `[A-Za-z_][A-Za-z0-9_]*` outside brokkr's `BROKKR_`, values
/// strings or finite numbers, no NUL; sorted by name.
fn env_at(state: &mut State, table: isize) -> Result<Vec<(String, String)>, Error> {
    if state.typ(table) != LuaType::Table || handle_index(state, table)?.is_some() {
        return Err(raise_failed(
            state,
            "binary_with(): env is a table of NAME = value".into(),
        ));
    }
    let mut env = Vec::new();
    state.push_nil()?;
    while state.table_next(table)? {
        let key_idx = top(state) - 1;
        let value_idx = top(state);
        if state.typ(key_idx) != LuaType::String {
            return Err(raise_failed(
                state,
                "binary_with(): env's keys are variable names (strings)".into(),
            ));
        }
        let name = text_at(state, key_idx, "an env name")?;
        let well_formed = name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !well_formed {
            return Err(raise_failed(
                state,
                format!("binary_with(): env name {name:?} is not [A-Za-z_][A-Za-z0-9_]*"),
            ));
        }
        if name.starts_with("BROKKR_") {
            return Err(raise_failed(
                state,
                format!(
                    "binary_with(): env name {name} is brokkr's: BROKKR_* carries brokkr's own channel to a child"
                ),
            ));
        }
        let what = format!("binary_with(): env {name}");
        let value = match state.typ(value_idx) {
            LuaType::String => text_at(state, value_idx, &what)?,
            LuaType::Number if state.to_number(value_idx)?.is_finite() => {
                number_text(state, value_idx)?
            }
            other => {
                return Err(raise_failed(
                    state,
                    format!(
                        "{what} is a {}: expected a string or a finite number",
                        other.as_str()
                    ),
                ));
            }
        };
        if value.contains('\0') {
            return Err(raise_failed(state, format!("{what} holds a NUL byte")));
        }
        env.push((name, value));
        state.pop(1)?;
    }
    state.pop(1)?;
    env.sort();
    Ok(env)
}

/// Run the call and push its result table.
fn call_binary<H: Host>(
    state: &mut State,
    function: &str,
    words: Vec<Word>,
    options: CallOptions,
) -> Result<u8, Error> {
    let ok = ctx::<H>(state).map(|c| c.ok);
    let external = options.program.is_some();
    let outcome = launch_host::<H>(state, function, external)?.binary(words, options);
    let outcome = outcome.map_err(|abort| raise(state, abort))?;
    state.new_table()?;
    let table = top(state);
    set_str(state, table, "program", outcome.program.as_bytes())?;
    if let Some(failure) = &outcome.start_error {
        set_str(state, table, "start_error", failure.message.as_bytes())?;
        set_str(
            state,
            table,
            "start_operation",
            failure.operation.as_bytes(),
        )?;
        if let Some(errno) = failure.errno {
            set_number(state, table, "start_errno", f64::from(errno))?;
        }
    }
    if let Some(code) = outcome.exit {
        set_number(state, table, "exit", f64::from(code))?;
    }
    if let Some(signal) = outcome.signal {
        set_number(state, table, "signal", f64::from(signal))?;
    }
    if outcome.start_error.is_none() {
        set_str(state, table, "stdout", &outcome.stdout)?;
        set_bool(state, table, "stdout_truncated", outcome.stdout_truncated)?;
        set_str(state, table, "stderr", &outcome.stderr)?;
        set_bool(state, table, "stderr_truncated", outcome.stderr_truncated)?;
    }
    if let Some(spool) = &outcome.stdout_file {
        state.push_string("stdout_file")?;
        push_handle(state, spool)?;
        state.set_table_raw(table)?;
    }
    if let Some(marker) = &outcome.stopped_at {
        set_str(state, table, "stopped_at", marker.as_bytes())?;
    }
    if let Some(capture) = &outcome.hotpath {
        set_str(state, table, "hotpath_status", capture.status.as_bytes())?;
        if let Some(report) = &capture.report {
            let marks = marks::<H>(state)?;
            state.push_string("hotpath")?;
            push_value(state, report, &marks)?;
            state.set_table_raw(table)?;
        }
        if let Some(error) = &capture.error {
            set_str(state, table, "hotpath_error", error.as_bytes())?;
        }
    }
    if let Some(ok) = ok {
        state.push_string("ok")?;
        state.push_rust_fn(ok)?;
        state.set_table_raw(table)?;
    }
    Ok(1)
}

fn set_str(state: &mut State, table: isize, key: &str, value: &[u8]) -> Result<(), Error> {
    state.push_string(key)?;
    state.push_bytes(value)?;
    state.set_table_raw(table)
}

fn set_number(state: &mut State, table: isize, key: &str, value: f64) -> Result<(), Error> {
    state.push_string(key)?;
    state.push_number(value)?;
    state.set_table_raw(table)
}

fn set_bool(state: &mut State, table: isize, key: &str, value: bool) -> Result<(), Error> {
    state.push_string(key)?;
    state.push_boolean(value)?;
    state.set_table_raw(table)
}

/// `read(h)`: the whole file a handle names, as a string.
fn lua_read<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    bench_host::<H>(state, "read")?;
    let handle = handle_arg(state, 1, "read")?;
    let read = bench_host::<H>(state, "read")?.read(handle);
    let bytes = read.map_err(|abort| raise(state, abort))?;
    state.push_bytes(&bytes)?;
    Ok(1)
}

/// `write(h, data)`: an output's file replaced by `data`.
fn lua_write<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    bench_host::<H>(state, "write")?;
    let handle = handle_arg(state, 1, "write")?;
    if state.typ(2) != LuaType::String {
        return Err(raise_failed(
            state,
            format!(
                "write()'s argument 2 is a {}: expected a string",
                state.typ(2).as_str()
            ),
        ));
    }
    let contents = state.to_bytes(2)?.to_vec();
    let written = bench_host::<H>(state, "write")?.write(handle, &contents);
    written.map_err(|abort| raise(state, abort))?;
    Ok(0)
}

/// `filesize(h)`: the size in bytes of the file a handle names.
fn lua_filesize<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    bench_host::<H>(state, "filesize")?;
    let handle = handle_arg(state, 1, "filesize")?;
    let size = bench_host::<H>(state, "filesize")?.filesize(handle);
    let size = size.map_err(|abort| raise(state, abort))?;
    if size > 1 << 53 {
        return Err(raise_failed(
            state,
            format!(
                "filesize(): {size} bytes is past 2^53, which a Lua number cannot hold exactly"
            ),
        ));
    }
    #[allow(clippy::cast_precision_loss)] // At most 2^53, exact.
    state.push_number(size as f64)?;
    Ok(1)
}

/// `json.encode(v)`: `v` as compact JSON text.
fn lua_json_encode<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    if top(state) < 1 {
        state.push_nil()?;
    }
    let marks = marks::<H>(state)?;
    let refused =
        |state: &State, why: String| raise_failed(state, format!("json.encode(): the value {why}"));
    let value = value_at(state, 1, 1, &marks).map_err(|why| refused(state, why))?;
    let text = json::encode(&value).map_err(|why| refused(state, why))?;
    state.push_bytes(&text)?;
    Ok(1)
}

/// `json.decode(s)`: one JSON document as Lua values, every array carrying
/// the array metatable and every null `json.null`.
fn lua_json_decode<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    if state.typ(1) != LuaType::String {
        return Err(raise_failed(
            state,
            format!(
                "json.decode()'s argument is a {}: expected a string",
                state.typ(1).as_str()
            ),
        ));
    }
    let text = state.to_bytes(1)?.to_vec();
    let value =
        json::decode(&text).map_err(|why| raise_failed(state, format!("json.decode(): {why}")))?;
    let marks = marks::<H>(state)?;
    push_value(state, &value, &marks)?;
    Ok(1)
}

fn empty(state: &mut State, table: isize) -> Result<bool, Error> {
    state.push_nil()?;
    if state.table_next(table)? {
        state.pop(2)?;
        Ok(false)
    } else {
        state.pop(1)?;
        Ok(true)
    }
}

/// `record` in the shape the store admits.
fn extract_record(
    state: &mut State,
    marks: &Marks,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    state
        .get_global("record")
        .map_err(|e| format!("cannot be read: {e}"))?;
    if state.typ(-1) != LuaType::Table {
        return Err(format!(
            "is a {}: it must stay a table",
            state.typ(-1).as_str()
        ));
    }
    let at = top(state);
    let value = value_at(state, at, 1, marks)?;
    json::record(&value)
}

/// The value at `idx` (absolute), nested `level` deep (the root is 1), as a
/// [`Value`]: a table with string keys a table, a marked array or a table
/// keyed 1..n a list, an empty plain table an empty table, `json.null` (and
/// nil) a null. A handle, a function, a non-finite number or a string that is
/// not UTF-8 is refused, and so is anything past [`json::MAX_DEPTH`], which
/// also ends a table that holds itself.
fn value_at(state: &mut State, idx: isize, level: usize, marks: &Marks) -> Result<Value, String> {
    let lua = |e: Error| format!("cannot be read: {e}");
    match state.typ(idx) {
        LuaType::Nil => Ok(Value::Null),
        LuaType::Boolean => Ok(Value::Bool(state.to_boolean(idx))),
        LuaType::Number => {
            let n = state.to_number(idx).map_err(lua)?;
            if n.is_finite() {
                Ok(Value::Number(n))
            } else {
                Err(format!("holds {n}, which JSON cannot"))
            }
        }
        LuaType::String => {
            let bytes = state.to_bytes(idx).map_err(lua)?.to_vec();
            String::from_utf8(bytes)
                .map(Value::Text)
                .map_err(|_| "holds a string that is not UTF-8".to_owned())
        }
        LuaType::Table => table_value(state, idx, level, marks),
        LuaType::Function => Err(
            "holds a function: only nil, booleans, numbers, strings and tables are stored"
                .to_owned(),
        ),
    }
}

fn table_value(
    state: &mut State,
    idx: isize,
    level: usize,
    marks: &Marks,
) -> Result<Value, String> {
    let lua = |e: Error| format!("cannot be read: {e}");
    // A handle or json.null is a value, not a container: judged before the
    // depth a container would add.
    let kind = marker(state, idx, Some(marks)).map_err(lua)?;
    if level > json::MAX_DEPTH && matches!(kind, Marker::Array | Marker::Plain) {
        return Err(format!(
            "nests deeper than {} levels or refers to itself",
            json::MAX_DEPTH
        ));
    }
    match kind {
        Marker::Handle(_) => {
            return Err("holds a handle: store or encode its path (h.path) instead".to_owned());
        }
        Marker::Null => {
            return if empty(state, idx).map_err(lua)? {
                Ok(Value::Null)
            } else {
                Err("holds a json.null that has fields set in it".to_owned())
            };
        }
        Marker::Array | Marker::Plain => {}
    }
    let mut object: Vec<(String, Value)> = Vec::new();
    let mut array: Vec<(usize, Value)> = Vec::new();
    state.push_nil().map_err(lua)?;
    while state.table_next(idx).map_err(lua)? {
        let key_idx = top(state) - 1;
        let value_idx = top(state);
        let value = value_at(state, value_idx, level + 1, marks)?;
        match state.typ(key_idx) {
            LuaType::String => {
                let bytes = state.to_bytes(key_idx).map_err(lua)?.to_vec();
                let key = String::from_utf8(bytes)
                    .map_err(|_| "has a key that is not UTF-8".to_owned())?;
                object.push((key, value));
            }
            LuaType::Number => {
                let n = state.to_number(key_idx).map_err(lua)?;
                let Some(position) = (n >= 1.0).then_some(n).and_then(exact_index) else {
                    return Err(format!("has a key {n} that is no position"));
                };
                array.push((position, value));
            }
            other => {
                return Err(format!(
                    "has a {} key: keys are strings, or positions 1..n",
                    other.as_str()
                ));
            }
        }
        state.pop(1).map_err(lua)?;
    }
    state.pop(1).map_err(lua)?;
    if !object.is_empty() && (kind == Marker::Array || !array.is_empty()) {
        return Err(if kind == Marker::Array {
            "has string keys in a JSON array".to_owned()
        } else {
            "mixes string keys and list positions in one table".to_owned()
        });
    }
    if array.is_empty() && kind == Marker::Plain {
        return Ok(Value::Table(object));
    }
    array.sort_by_key(|(position, _)| *position);
    if array.iter().enumerate().any(|(i, (p, _))| *p != i + 1) {
        return Err("has a list with gaps: positions must run 1..n".to_owned());
    }
    Ok(Value::List(array.into_iter().map(|(_, v)| v).collect()))
}

/// `result:ok()`: the result itself when the child exited 0 or was stopped
/// at its `stop_at` marker; otherwise the
/// sample fails, naming how the child ended and its last stderr line.
fn lua_result_ok<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    if state.typ(1) != LuaType::Table {
        return Err(raise_failed(
            state,
            "ok() is a method of a binary() result: call it as r:ok()".into(),
        ));
    }
    let field = |state: &mut State, key: &str| -> Result<(), Error> {
        state.push_string(key)?;
        state.get_table_raw(1)
    };
    field(state, "exit")?;
    let exit = (state.typ(-1) == LuaType::Number)
        .then(|| state.to_number(-1))
        .transpose()?;
    state.pop(1)?;
    field(state, "stopped_at")?;
    let stopped = state.typ(-1) == LuaType::String;
    state.pop(1)?;
    if exit == Some(0.0) || stopped {
        state.push_value(1)?;
        return Ok(1);
    }
    field(state, "program")?;
    let program = if state.typ(-1) == LuaType::String {
        String::from_utf8_lossy(state.to_bytes(-1)?).into_owned()
    } else {
        "the child".to_owned()
    };
    state.pop(1)?;
    field(state, "start_error")?;
    if state.typ(-1) == LuaType::String {
        // The message names the operation already (`resolve: ...`).
        let why = String::from_utf8_lossy(state.to_bytes(-1)?).into_owned();
        state.pop(1)?;
        return Err(raise(
            state,
            Abort::Fail(format!("{program} could not start: {why}")),
        ));
    }
    state.pop(1)?;
    field(state, "signal")?;
    let signal = (state.typ(-1) == LuaType::Number)
        .then(|| state.to_number(-1))
        .transpose()?;
    state.pop(1)?;
    field(state, "stderr")?;
    let stderr = if state.typ(-1) == LuaType::String {
        String::from_utf8_lossy(state.to_bytes(-1)?).into_owned()
    } else {
        String::new()
    };
    state.pop(1)?;
    let ending = match (exit, signal) {
        (Some(code), _) => format!("exited {code}"),
        (None, Some(signal)) => format!("was killed by signal {signal}"),
        (None, None) => "did not exit".to_owned(),
    };
    let mut why = format!("{program} {ending}");
    if let Some(line) = stderr.lines().rev().find(|l| !l.trim().is_empty()) {
        why.push_str(": ");
        why.push_str(line.trim());
    }
    // The script's own verdict on what the child did, as `fail()` is.
    Err(raise(state, Abort::Fail(why)))
}

/// `print`, routed through brokkr's console channel rather than straight to
/// brokkr's stdout as dellingr's default callbacks would.
struct Console;

impl ::dellingr::HostCallbacks for Console {
    fn on_print(&mut self, _source: Option<&[u8]>, _line: u32, message: &[u8]) {
        crate::output::bench_msg(&String::from_utf8_lossy(message));
    }
}

#[cfg(test)]
#[path = "lua_tests.rs"]
mod tests;
