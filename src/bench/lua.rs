//! The seam to the linked dellingr crate, the Lua VM script benches run on.
//!
//! brokkr has a module of its own named `dellingr` (the `brokkr dellingr`
//! command), so the crate is reached only here, by its absolute `::dellingr`
//! path; a textlint rule keeps every other file off it, and a change in the
//! crate's API touches this file alone.
//!
//! Two uses: [`compile`] for `brokkr check`'s `bench` phase, which runs
//! nothing, and [`run_sample`], one iteration of a script bench in a fresh
//! State. Every builtin the script sees is marshalled here and decided by the
//! runner through [`Host`], whose types carry no dellingr value: Lua strings
//! cross as bytes, text fields are checked from those exact bytes, and a
//! builtin's failure travels back through Lua as an [`Abort`] inside a host
//! error, recovered by downcast. dellingr 0.6.0 has no interrupt, so a script
//! spinning in pure Lua is stopped only at its next builtin call.

use std::time::{Duration, Instant};

use ::dellingr::error::{Error, ErrorKind};
use ::dellingr::{ArgCount, LuaType, RetCount, RustFnHandle, State};

/// Why a script did not compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompileError {
    /// 1-based line of the failure in the script, 0 when unknown.
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
pub(crate) fn compile(source: &[u8]) -> Result<(), CompileError> {
    load_script(source, None).map(|_| ())
}

/// A compiled script, run unchanged in every iteration of a run.
#[derive(Clone)]
pub(crate) struct Script(::dellingr::Program);

/// Compile `source` under `chunk`, the name errors and tracebacks give it.
pub(crate) fn load_script(source: &[u8], chunk: Option<&str>) -> Result<Script, CompileError> {
    ::dellingr::Program::compile(source, chunk.map(str::as_bytes))
        .map(Script)
        .map_err(|error| CompileError {
            line: error.line_num(),
            message: error.kind().to_string(),
        })
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

/// A value the runner hands the script, built without dellingr: the params,
/// their keys, `config()`'s table. A table's keys are strings; a list is
/// positions 1..n.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
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

/// What one `binary()` call hands back to the script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallOutcome {
    /// The program's file name, for messages (`result:ok()`).
    pub(crate) program: String,
    pub(crate) exit: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stderr_truncated: bool,
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
/// decided here too (`binary`, `output` and `scratch` raise there).
pub(crate) trait Host: Send + 'static {
    /// `bench()` is about to be called: the block starts now.
    fn begin_block(&mut self, at: Instant);
    fn binary(&mut self, words: Vec<Word>) -> Result<CallOutcome, Abort>;
    fn since(&mut self, sha: &str) -> Result<bool, Abort>;
    fn output(&mut self, name: &str) -> Result<HandleRef, Abort>;
    fn scratch(&mut self, name: &str) -> Result<HandleRef, Abort>;
    /// The resolved config, for `config()`; allowed at top level.
    fn config(&mut self) -> Result<std::sync::Arc<Value>, Abort>;
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

/// The deepest a record may nest: the store refuses past 64 levels.
const MAX_RECORD_DEPTH: usize = 64;

/// The hidden field a handle table carries its index in.
const HANDLE_KEY: &str = "__brokkr_handle";

/// The per-State context a builtin reaches through the State's user data.
struct Ctx<H> {
    host: H,
    in_bench: bool,
    ok: RustFnHandle,
}

/// Run one sample of `script` in a fresh State: the chunk's top level, then
/// `bench(params, keys)` timed from its call to its return, then `record`
/// extracted. Returns the host for the next sample.
pub(crate) fn run_sample<H: Host>(script: &Script, args: &Args, host: H) -> (Sample, H) {
    let mut state = State::with_callbacks(Box::new(Console));
    let ok = match install::<H>(&mut state) {
        Ok(ok) => ok,
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
    }));
    let sample = sample::<H>(&mut state, script, args);
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

fn sample<H: Host>(state: &mut State, script: &Script, args: &Args) -> Sample {
    if let Err(error) = state
        .load(&script.0)
        .and_then(|()| state.call(ArgCount::Fixed(0), RetCount::Fixed(0)))
    {
        return ended(&error, None);
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
    if let Err(error) = push_value(state, &Value::Table(args.params.clone())) {
        return failed(format!("cannot make the params table: {error}"), None);
    }
    if let Err(error) = push_value(state, &Value::Table(args.keys.clone())) {
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
        return ended(&error, Some(wall));
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
    match extract_record(state) {
        Ok(record) => Sample {
            outcome: SampleOutcome::Completed(verdict),
            wall: Some(wall),
            record: Some(record),
        },
        Err(why) => failed(format!("record {why}"), Some(wall)),
    }
}

/// A sample ended by an error out of the chunk or `bench()`.
fn ended(error: &Error, wall: Option<Duration>) -> Sample {
    let outcome = match abort_of(error) {
        Some(Abort::Interrupted) => SampleOutcome::Interrupted,
        Some(Abort::Failed(why)) => SampleOutcome::Failed(why.clone()),
        None => {
            let shown = error.to_string();
            SampleOutcome::Failed(if shown.trim().is_empty() {
                format!("the script failed ({:?})", error.kind())
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

fn abort_of(error: &Error) -> Option<&Abort> {
    match error.kind() {
        ErrorKind::HostError(inner) => inner.downcast_ref::<Abort>(),
        _ => None,
    }
}

/// The builtins, as globals (so dellingr names them in errors), and the
/// `record` table. Returns the handle of `result:ok`.
fn install<H: Host>(state: &mut State) -> Result<RustFnHandle, Error> {
    let builtins: [(&str, ::dellingr::RustFunc); 6] = [
        ("binary", lua_binary::<H>),
        ("since", lua_since::<H>),
        ("fail", lua_fail),
        ("output", lua_output::<H>),
        ("scratch", lua_scratch::<H>),
        ("config", lua_config::<H>),
    ];
    for (name, function) in builtins {
        let handle = state.add_rust_fn(function)?;
        state.push_rust_fn(handle)?;
        state.set_global(name)?;
    }
    state.new_table()?;
    state.set_global("record")?;
    state.add_rust_fn(lua_result_ok)
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
    state.check_type(idx, LuaType::String)?;
    text_at(state, idx, &format!("{function}()'s argument"))
}

fn lua_since<H: Host>(state: &mut State) -> Result<u8, Error> {
    let sha = string_arg(state, 1, "since")?;
    let answer = match ctx::<H>(state) {
        Some(ctx) => ctx.host.since(&sha),
        None => return Err(raise_failed(state, "since(): no bench context".into())),
    };
    match answer {
        Ok(yes) => {
            state.push_boolean(yes)?;
            Ok(1)
        }
        Err(abort) => Err(raise(state, abort)),
    }
}

fn lua_fail(state: &mut State) -> Result<u8, Error> {
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
    let name = string_arg(state, 1, function)?;
    let made = make(bench_host::<H>(state, function)?, &name);
    let handle = made.map_err(|abort| raise(state, abort))?;
    push_handle(state, &handle)?;
    Ok(1)
}

/// A handle's table: `path`, and its index under the hidden key.
fn push_handle(state: &mut State, handle: &HandleRef) -> Result<(), Error> {
    state.new_table()?;
    let table = top(state);
    state.push_string("path")?;
    state.push_string(&handle.path)?;
    state.set_table_raw(table)?;
    state.push_string(HANDLE_KEY)?;
    state.push_number(f64::from(u32::try_from(handle.index).unwrap_or(u32::MAX)))?;
    state.set_table_raw(table)
}

/// Push `value` as a fresh Lua value. A table with the hidden handle key is
/// refused, so config data can never pass for a handle the runner made.
fn push_value(state: &mut State, value: &Value) -> Result<(), Error> {
    match value {
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
                push_value(state, item)?;
                state.set_table_raw(table)?;
            }
            Ok(())
        }
        Value::Table(fields) => {
            state.new_table()?;
            let table = top(state);
            for (key, item) in fields {
                if key == HANDLE_KEY {
                    return Err(raise_failed(
                        state,
                        format!("the key {HANDLE_KEY} is brokkr's: config data cannot use it"),
                    ));
                }
                state.push_string(key)?;
                push_value(state, item)?;
                state.set_table_raw(table)?;
            }
            Ok(())
        }
    }
}

/// `config()`: a fresh copy of the resolved config each call.
fn lua_config<H: Host>(state: &mut State) -> Result<u8, Error> {
    let view = match ctx::<H>(state) {
        Some(ctx) => ctx.host.config(),
        None => return Err(raise_failed(state, "config(): no bench context".into())),
    };
    let view = view.map_err(|abort| raise(state, abort))?;
    push_value(state, &view)?;
    Ok(1)
}

/// The handle index a table at `idx` carries, if it is a handle.
fn handle_index(state: &mut State, idx: isize) -> Result<Option<usize>, Error> {
    state.push_string(HANDLE_KEY)?;
    state.get_table_raw(idx)?;
    let index = if state.typ(-1) == LuaType::Number {
        let n = state.to_number(-1)?;
        (n >= 0.0 && n.fract() == 0.0 && n < 4_294_967_296.0).then(|| {
            // An exact non-negative integer below 2^32.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = n as usize;
            n
        })
    } else {
        None
    };
    state.pop(1)?;
    Ok(index)
}

/// One argv word from a string, number or handle at `idx`.
fn word_at(state: &mut State, idx: isize, what: &str) -> Result<Word, Error> {
    match state.typ(idx) {
        LuaType::String => Ok(Word::Text(text_at(state, idx, what)?)),
        LuaType::Number => {
            let rendered = state.tostring_bytes(idx)?;
            Ok(Word::Text(String::from_utf8_lossy(&rendered).into_owned()))
        }
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
                Flag::Many(list_at(state, value_idx, &what)?)
            }
            _ => Flag::One(word_at(state, value_idx, &what)?),
        };
        flags.push((key, flag));
        state.pop(1)?;
    }
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

/// A flag's list: exactly the positions 1..n, each a string, number or
/// handle, in order. A named key or a gap is refused rather than dropped.
fn list_at(state: &mut State, table: isize, what: &str) -> Result<Vec<Word>, Error> {
    let mut items: Vec<(usize, Word)> = Vec::new();
    state.push_nil()?;
    while state.table_next(table)? {
        let key_idx = top(state) - 1;
        let value_idx = top(state);
        let position = (state.typ(key_idx) == LuaType::Number)
            .then(|| state.to_number(key_idx))
            .transpose()?
            .filter(|n| *n >= 1.0 && n.fract() == 0.0 && *n < 4_294_967_296.0);
        let Some(position) = position else {
            return Err(raise_failed(
                state,
                format!("{what} is a table that is neither a handle nor a list 1..n"),
            ));
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let position = position as usize;
        let item = word_at(state, value_idx, &format!("{what}[{position}]"))?;
        items.push((position, item));
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

fn lua_binary<H: Host>(state: &mut State) -> Result<u8, Error> {
    let count = top(state);
    let mut words = Vec::new();
    for idx in 1..=count {
        let what = format!("binary()'s argument {idx}");
        if state.typ(idx) == LuaType::Table && handle_index(state, idx)?.is_none() {
            flags_at(state, idx, &mut words)?;
        } else {
            words.push(word_at(state, idx, &what)?);
        }
    }
    let ok = ctx::<H>(state).map(|c| c.ok);
    let outcome = bench_host::<H>(state, "binary")?.binary(words);
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

/// `result:ok()`: the result itself when the child exited 0; otherwise the
/// sample fails, naming how the child ended and its last stderr line.
fn lua_result_ok(state: &mut State) -> Result<u8, Error> {
    state.check_type(1, LuaType::Table)?;
    let field = |state: &mut State, key: &str| -> Result<(), Error> {
        state.push_string(key)?;
        state.get_table_raw(1)
    };
    field(state, "exit")?;
    let exit = (state.typ(-1) == LuaType::Number)
        .then(|| state.to_number(-1))
        .transpose()?;
    state.pop(1)?;
    if exit == Some(0.0) {
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

/// `record` as a JSON object, in the shape the store admits.
fn extract_record(state: &mut State) -> Result<serde_json::Map<String, serde_json::Value>, String> {
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
    let value = table_json(state, at, 1, true).map_err(|e| e.to_string())?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err("must be a table with string keys".to_owned()),
    }
}

/// The table at `idx`, nested `level` deep (the root is 1), as JSON: string
/// keys an object, keys 1..n an array (below the root only), an empty table
/// an empty object.
fn table_json(
    state: &mut State,
    idx: isize,
    level: usize,
    root: bool,
) -> Result<serde_json::Value, RecordError> {
    if level > MAX_RECORD_DEPTH {
        return Err(RecordError(format!(
            "nests deeper than {MAX_RECORD_DEPTH} levels or refers to itself"
        )));
    }
    let mut object = serde_json::Map::new();
    let mut array: Vec<(usize, serde_json::Value)> = Vec::new();
    state.push_nil().map_err(RecordError::lua)?;
    while state.table_next(idx).map_err(RecordError::lua)? {
        let key_idx = top(state) - 1;
        let value_idx = top(state);
        let value = value_json(state, value_idx, level)?;
        match state.typ(key_idx) {
            LuaType::String => {
                let bytes = state.to_bytes(key_idx).map_err(RecordError::lua)?.to_vec();
                let key = String::from_utf8(bytes)
                    .map_err(|_| RecordError("has a key that is not UTF-8".to_owned()))?;
                object.insert(key, value);
            }
            LuaType::Number => {
                let n = state.to_number(key_idx).map_err(RecordError::lua)?;
                if !(n >= 1.0 && n.fract() == 0.0 && n < 4_294_967_296.0) {
                    return Err(RecordError(format!("has a key {n} that is no position")));
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let position = n as usize;
                array.push((position, value));
            }
            other => {
                return Err(RecordError(format!(
                    "has a {} key: keys are strings, or positions 1..n",
                    other.as_str()
                )));
            }
        }
        state.pop(1).map_err(RecordError::lua)?;
    }
    state.pop(1).map_err(RecordError::lua)?;
    match (object.is_empty(), array.is_empty()) {
        (_, true) => Ok(serde_json::Value::Object(object)),
        (true, false) if !root => {
            array.sort_by_key(|(position, _)| *position);
            if array.iter().enumerate().any(|(i, (p, _))| *p != i + 1) {
                return Err(RecordError(
                    "has a list with gaps: positions must run 1..n".to_owned(),
                ));
            }
            Ok(serde_json::Value::Array(
                array.into_iter().map(|(_, v)| v).collect(),
            ))
        }
        (true, false) => Err(RecordError(
            "must be a table with string keys, not a list".to_owned(),
        )),
        (false, false) => Err(RecordError(
            "mixes string keys and list positions in one table".to_owned(),
        )),
    }
}

fn value_json(
    state: &mut State,
    idx: isize,
    level: usize,
) -> Result<serde_json::Value, RecordError> {
    match state.typ(idx) {
        LuaType::Nil => Ok(serde_json::Value::Null),
        LuaType::Boolean => Ok(serde_json::Value::Bool(state.to_boolean(idx))),
        LuaType::Number => {
            let n = state.to_number(idx).map_err(RecordError::lua)?;
            if !n.is_finite() {
                return Err(RecordError(format!("holds {n}, which JSON cannot")));
            }
            if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
                #[allow(clippy::cast_possible_truncation)]
                let int = n as i64;
                Ok(serde_json::Value::from(int))
            } else {
                serde_json::Number::from_f64(n)
                    .map(serde_json::Value::Number)
                    .ok_or_else(|| RecordError(format!("holds {n}, which JSON cannot")))
            }
        }
        LuaType::String => {
            let bytes = state.to_bytes(idx).map_err(RecordError::lua)?.to_vec();
            String::from_utf8(bytes)
                .map(serde_json::Value::String)
                .map_err(|_| RecordError("holds a string that is not UTF-8".to_owned()))
        }
        LuaType::Table => table_json(state, idx, level + 1, false),
        LuaType::Function => Err(RecordError(
            "holds a function: only nil, booleans, numbers, strings and tables are stored"
                .to_owned(),
        )),
    }
}

/// Why `record` could not be stored.
#[derive(Debug)]
struct RecordError(String);

impl RecordError {
    #[allow(clippy::needless_pass_by_value)] // `map_err` hands the error over.
    fn lua(error: Error) -> Self {
        Self(format!("cannot be read: {error}"))
    }
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
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
