//! The seam to the linked dellingr crate, the Lua VM script benches run on.
//!
//! The crate is reached only here, by its absolute `::dellingr` path; a
//! textlint rule keeps every other file off it, and a change in the crate's
//! API touches this file alone.
//!
//! Two uses: [`compile`] for `brokkr check`'s `bench` phase, which runs
//! nothing, and [`run_sample`], one iteration of a script bench in a fresh
//! State. Every builtin the script sees is marshalled here and decided by the
//! runner through [`Host`], whose types carry no dellingr value: Lua strings
//! cross as bytes, text fields are checked from those exact bytes, and a
//! builtin's failure travels back through Lua as an [`Abort`] inside a host
//! error, recovered by downcast. dellingr 0.6.0 has no interrupt, so a script
//! spinning in pure Lua is stopped only at its next builtin call: every
//! builtin but `print` asks the host for a pending stop before anything else.
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

/// Compile `source` without running any of it. Only what the parser and
/// code generator reject is caught: a call to a misspelt builtin compiles.
pub(crate) fn compile(source: &[u8], lines: Option<&LineMap>) -> Result<(), CompileError> {
    load_script(source, None, lines.cloned()).map(|_| ())
}

/// A compiled script, run unchanged in every iteration of a run, with the
/// map of its lines into the config file that holds it, for an inline one.
#[derive(Clone)]
pub(crate) struct Script {
    program: ::dellingr::Program,
    lines: Option<Arc<LineMap>>,
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
            program,
            lines: lines.clone(),
        })
        .map_err(|error| CompileError {
            line: mapped(lines.as_deref(), error.line_num()),
            message: error.kind().to_string(),
        })
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
    /// The sample fails with this reason (never empty).
    Failed(String),
}

impl std::fmt::Display for Abort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interrupted => f.write_str("interrupted"),
            Self::Failed(why) => f.write_str(why),
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
}

/// The metatable key a handle carries its index under.
const HANDLE_MARK: &str = "__brokkr_handle";

/// The per-State context a builtin reaches through the State's user data.
struct Ctx<H> {
    host: H,
    in_bench: bool,
    ok: RustFnHandle,
    marks: Marks,
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
    let mut state = State::builder().callbacks(Box::new(Console)).build();
    let (ok, marks) = match install::<H>(&mut state) {
        Ok(installed) => installed,
        Err(error) => {
            return (
                failed(format!("cannot set up the Lua state: {error}"), None),
                host,
            );
        }
    };
    state.set_user_data(Some(Ctx {
        host,
        in_bench: false,
        ok,
        marks: marks.clone(),
    }));
    let sample = sample::<H>(&mut state, script, args, &marks);
    let host = state
        .user_data_mut::<Option<Ctx<H>>>()
        .and_then(Option::take)
        .map(|ctx| ctx.host);
    match host {
        Some(host) => (sample, host),
        // The context is set above and only taken here.
        None => unreachable!("the sample's host context went missing"),
    }
}

fn failed(reason: String, wall: Option<Duration>) -> Sample {
    Sample {
        outcome: SampleOutcome::Failed(reason),
        wall,
        record: None,
    }
}

fn sample<H: Host>(state: &mut State, script: &Script, args: &Args, marks: &Marks) -> Sample {
    let lines = script.lines.as_deref();
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
        ctx.in_bench = true;
        ctx.host.begin_block(start);
    }
    let called = state.call(ArgCount::Fixed(2), RetCount::Fixed(1));
    let wall = start.elapsed();
    if let Some(ctx) = ctx::<H>(state) {
        ctx.in_bench = false;
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
        },
        Err(why) => failed(format!("record {why}"), Some(wall)),
    }
}

/// A sample ended by an error out of the chunk or `bench()`. A builtin's
/// failure is its reason after the script line that called it; any other
/// error is dellingr's own rendering, its traceback included, with every
/// line mapped.
fn ended(error: Error, lines: Option<&LineMap>, wall: Option<Duration>) -> Sample {
    let outcome = match abort_of(&error) {
        Some(Abort::Interrupted) => SampleOutcome::Interrupted,
        Some(Abort::Failed(why)) => SampleOutcome::Failed(located(&error, lines, why)),
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
    }
}

/// `why` after `chunk:line:` of the innermost script frame, or alone when no
/// frame knows its line.
fn located(error: &Error, lines: Option<&LineMap>, why: &str) -> String {
    let frame = error
        .stack_trace()
        .iter()
        .find(|f| f.source.is_some() && f.line != 0);
    match frame.and_then(|f| f.source.as_deref().map(|s| (s, f.line))) {
        Some((source, line)) => format!(
            "{}:{}: {why}",
            String::from_utf8_lossy(source),
            mapped(lines, line)
        ),
        None => why.to_owned(),
    }
}

/// dellingr's rendering of `error` with every frame's lines mapped.
fn rendered(error: Error, lines: Option<&LineMap>) -> String {
    let Some(lines) = lines else {
        return error.to_string();
    };
    if error.stack_trace().is_empty() {
        // Rendered from the error's own position, which no frame carries.
        let at = mapped(Some(lines), error.line_num());
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
            line: mapped(Some(lines), frame.line),
            defined_line: mapped(Some(lines), frame.defined_line),
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
    let builtins: [(&str, ::dellingr::RustFunc); 10] = [
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
    let in_bench = ctx::<H>(state).is_some_and(|c| c.in_bench);
    if !in_bench {
        return Err(raise_failed(
            state,
            format!("{name}() is only available inside bench(), not at the script's top level"),
        ));
    }
    match ctx::<H>(state) {
        Some(ctx) => Ok(&mut ctx.host),
        None => Err(raise_failed(state, format!("{name}(): no bench context"))),
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
    let answer = any_host::<H>(state, "since")?.since(&sha);
    match answer {
        Ok(yes) => {
            state.push_boolean(yes)?;
            Ok(1)
        }
        Err(abort) => Err(raise(state, abort)),
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
    Err(raise_failed(state, reason))
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
    bench_host::<H>(state, "binary")?;
    let words = words_from(state, 1)?;
    call_binary::<H>(state, "binary", words, CallOptions::default())
}

fn lua_binary_with<H: Host>(state: &mut State) -> Result<u8, Error> {
    stop_first::<H>(state)?;
    bench_host::<H>(state, "binary_with")?;
    let options = options_at(state, 1)?;
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
                 (env, reads, writes, stdin, hotpath, stop_at)",
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
                         stdin, hotpath and stop_at"
                    ),
                ));
            }
        }
        state.pop(1)?;
    }
    state.pop(1)?;
    if options.hotpath.is_some() && options.stop_at.is_some() {
        return Err(raise_failed(
            state,
            "binary_with(): hotpath and stop_at exclude each other: a child stopped at a \
             marker is killed and writes no report"
                .into(),
        ));
    }
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
            format!("{what} is a {}: expected a marker name", state.typ(idx).as_str()),
        ));
    }
    let marker = text_at(state, idx, what)?;
    // Letters, digits and `_` only: a leading `@` or `%` is the FIFO's
    // counter or call-tag syntax, never a process-stream marker.
    let exact = marker.len() > "_END".len()
        && marker.ends_with("_END")
        && marker.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
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
    let outcome = bench_host::<H>(state, function)?.binary(words, options);
    let outcome = outcome.map_err(|abort| raise(state, abort))?;
    state.new_table()?;
    let table = top(state);
    set_str(state, table, "program", outcome.program.as_bytes())?;
    if let Some(code) = outcome.exit {
        set_number(state, table, "exit", f64::from(code))?;
    }
    if let Some(signal) = outcome.signal {
        set_number(state, table, "signal", f64::from(signal))?;
    }
    set_str(state, table, "stdout", &outcome.stdout)?;
    set_bool(state, table, "stdout_truncated", outcome.stdout_truncated)?;
    set_str(state, table, "stderr", &outcome.stderr)?;
    set_bool(state, table, "stderr_truncated", outcome.stderr_truncated)?;
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
    Err(raise_failed(state, why))
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
