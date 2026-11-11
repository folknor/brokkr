//! A `run(spec)` table read strictly: every key known, every value its
//! shape, so a misspelt fixture fails the test rather than quietly testing
//! less (`brokkr man bench`, Contract tests).

use std::collections::{BTreeMap, VecDeque};

use crate::bench::lua::Value;

/// One `run()`'s fixtures.
#[derive(Debug, Default)]
pub(super) struct Spec {
    pub(super) bench: String,
    /// The tokens after NAME on a command line.
    pub(super) params: Vec<String>,
    /// The build's file name, for a bench that names no `target`.
    pub(super) program: Option<String>,
    /// The synthetic host's section, as TOML; `None` is a host with none.
    pub(super) host: Option<toml::Table>,
    pub(super) calls: VecDeque<Response>,
    pub(super) preflight_calls: VecDeque<PreflightResponse>,
    pub(super) since: BTreeMap<String, bool>,
    /// The contents of each input, by its path as the script sees it.
    pub(super) files: BTreeMap<String, Vec<u8>>,
    /// `page_cache()`'s answer for an input path: why it cannot be
    /// prepared; absent, its prerequisites hold.
    pub(super) page_cache: BTreeMap<String, String>,
}

/// How a virtual child ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ending {
    Exit(i32),
    Signal(i32),
    /// At this `stop_at` marker, which the call must have asked for.
    StoppedAt(String),
}

/// What one call of `bench()` answers.
#[derive(Debug, Clone)]
pub(super) struct Response {
    pub(super) ending: Ending,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    /// Files the child writes, by the name the script gave `output()`.
    pub(super) outputs: BTreeMap<String, Vec<u8>>,
    /// The hotpath report a marked call captured; a marked call without one
    /// captured nothing (`missing`).
    pub(super) hotpath: Option<Value>,
}

/// What one call of `preflight()` answers.
#[derive(Debug, Clone)]
pub(super) struct PreflightResponse {
    pub(super) ending: PreflightEnding,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PreflightEnding {
    Exit(i32),
    Signal(i32),
    /// The program never started: `resolve` or `execute`, and the message.
    StartError {
        operation: &'static str,
        message: String,
    },
}

/// The fields of a fixture table, each taken once; whatever is left over is
/// a key nothing reads.
struct Fields {
    what: String,
    fields: Vec<(String, Value)>,
}

impl Fields {
    fn of(value: Value, what: &str) -> Result<Self, String> {
        match value {
            Value::Table(fields) => Ok(Self {
                what: what.to_owned(),
                fields,
            }),
            // An empty Lua table reads as an empty list.
            Value::List(items) if items.is_empty() => Ok(Self {
                what: what.to_owned(),
                fields: Vec::new(),
            }),
            other => Err(format!("{what} is {}: expected a table", shape(&other))),
        }
    }

    fn take(&mut self, key: &str) -> Option<Value> {
        let at = self.fields.iter().position(|(k, _)| k == key)?;
        Some(self.fields.remove(at).1)
    }

    fn text(&mut self, key: &str) -> Result<Option<String>, String> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Text(text)) => Ok(Some(text)),
            Some(other) => Err(format!(
                "{}.{key} is {}: expected a string",
                self.what,
                shape(&other)
            )),
        }
    }

    fn bytes(&mut self, key: &str) -> Result<Vec<u8>, String> {
        Ok(self.text(key)?.unwrap_or_default().into_bytes())
    }

    fn code(&mut self, key: &str) -> Result<Option<i32>, String> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Number(n)) if n.fract() == 0.0 && n.abs() <= f64::from(i32::MAX) => {
                #[allow(clippy::cast_possible_truncation)] // Integral and in range.
                Ok(Some(n as i32))
            }
            Some(other) => Err(format!(
                "{}.{key} is {}: expected an integer",
                self.what,
                shape(&other)
            )),
        }
    }

    fn list(&mut self, key: &str) -> Result<Vec<Value>, String> {
        match self.take(key) {
            None => Ok(Vec::new()),
            Some(Value::List(items)) => Ok(items),
            Some(Value::Table(fields)) if fields.is_empty() => Ok(Vec::new()),
            Some(other) => Err(format!(
                "{}.{key} is {}: expected a list",
                self.what,
                shape(&other)
            )),
        }
    }

    fn map(&mut self, key: &str) -> Result<Vec<(String, Value)>, String> {
        match self.take(key) {
            None => Ok(Vec::new()),
            Some(Value::Table(fields)) => Ok(fields),
            Some(Value::List(items)) if items.is_empty() => Ok(Vec::new()),
            Some(other) => Err(format!(
                "{}.{key} is {}: expected a table keyed by name",
                self.what,
                shape(&other)
            )),
        }
    }

    /// Refuse every key no reader took.
    fn done(self) -> Result<(), String> {
        match self.fields.first() {
            None => Ok(()),
            Some((key, _)) => Err(format!(
                "{} has no key {key}: a misspelt fixture would test less than it says",
                self.what
            )),
        }
    }
}

fn shape(value: &Value) -> &'static str {
    match value {
        Value::Null => "nil",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::Text(_) => "a string",
        Value::List(_) => "a list",
        Value::Table(_) => "a table",
        Value::Handle(_) => "a handle",
    }
}

pub(super) fn parse(spec: Value) -> Result<Spec, String> {
    let mut fields = Fields::of(spec, "the spec")?;
    let bench = fields
        .text("bench")?
        .ok_or_else(|| "the spec names no bench: run({bench = NAME, ...})".to_owned())?;
    let params = fields
        .list("params")?
        .into_iter()
        .enumerate()
        .map(|(i, token)| match token {
            Value::Text(text) => Ok(text),
            other => Err(format!(
                "the spec's params[{}] is {}: params are the command line's words, strings",
                i + 1,
                shape(&other)
            )),
        })
        .collect::<Result<_, _>>()?;
    let program = fields.text("program")?;
    if let Some(name) = &program
        && (name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']))
    {
        return Err(format!(
            "the spec's program {name:?} is no file name: it is the build's, as a binary() \
             result names it"
        ));
    }
    let host = match fields.take("host") {
        None => None,
        Some(value) => match toml_value(&value, "the spec's host")? {
            toml::Value::Table(table) => Some(table),
            _ => return Err("the spec's host is not a table".into()),
        },
    };
    let calls = fields
        .list("calls")?
        .into_iter()
        .enumerate()
        .map(|(i, call)| response(call, &format!("calls[{}]", i + 1)))
        .collect::<Result<_, _>>()?;
    let preflight_calls = fields
        .list("preflight_calls")?
        .into_iter()
        .enumerate()
        .map(|(i, call)| preflight_response(call, &format!("preflight_calls[{}]", i + 1)))
        .collect::<Result<_, _>>()?;
    let since = fields
        .map("since")?
        .into_iter()
        .map(|(sha, answer)| match answer {
            Value::Bool(yes) => Ok((sha, yes)),
            other => Err(format!(
                "since[{sha:?}] is {}: expected true or false",
                shape(&other)
            )),
        })
        .collect::<Result<_, _>>()?;
    let files = fields
        .map("files")?
        .into_iter()
        .map(|(path, contents)| match contents {
            Value::Text(text) => Ok((path, text.into_bytes())),
            other => Err(format!(
                "files[{path:?}] is {}: expected the file's contents",
                shape(&other)
            )),
        })
        .collect::<Result<_, _>>()?;
    let page_cache = fields
        .map("page_cache")?
        .into_iter()
        .map(|(path, why)| match why {
            Value::Text(why) if !why.trim().is_empty() => Ok((path, why)),
            other => Err(format!(
                "page_cache[{path:?}] is {}: expected why it cannot be prepared (omit the \
                 path when it can)",
                shape(&other)
            )),
        })
        .collect::<Result<_, _>>()?;
    fields.done()?;
    Ok(Spec {
        bench,
        params,
        program,
        host,
        calls,
        preflight_calls,
        since,
        files,
        page_cache,
    })
}

/// A child's ending from `exit`, `signal` and, for a `bench()` call,
/// `stopped_at`: at most one, an exit of 0 when none is given.
fn ending(fields: &mut Fields, stops: bool) -> Result<Ending, String> {
    let exit = fields.code("exit")?;
    let signal = fields.code("signal")?;
    let stopped = if stops {
        fields.text("stopped_at")?
    } else {
        None
    };
    match (exit, signal, stopped) {
        (None, None, None) => Ok(Ending::Exit(0)),
        (Some(code), None, None) => Ok(Ending::Exit(code)),
        (None, Some(signal), None) => Ok(Ending::Signal(signal)),
        (None, None, Some(marker)) => Ok(Ending::StoppedAt(marker)),
        _ => Err(format!(
            "{} gives more than one of exit, signal and stopped_at: a child ends one way",
            fields.what
        )),
    }
}

fn response(value: Value, what: &str) -> Result<Response, String> {
    let mut fields = Fields::of(value, what)?;
    let ending = ending(&mut fields, true)?;
    let stdout = fields.bytes("stdout")?;
    let stderr = fields.bytes("stderr")?;
    let outputs = fields
        .map("outputs")?
        .into_iter()
        .map(|(name, contents)| match contents {
            Value::Text(text) => Ok((name, text.into_bytes())),
            other => Err(format!(
                "{what}.outputs[{name:?}] is {}: expected the file's contents",
                shape(&other)
            )),
        })
        .collect::<Result<_, _>>()?;
    let hotpath = match fields.take("hotpath") {
        None => None,
        Some(report @ Value::Table(_)) => Some(report),
        Some(other) => {
            return Err(format!(
                "{what}.hotpath is {}: expected the captured report, a table",
                shape(&other)
            ));
        }
    };
    fields.done()?;
    Ok(Response {
        ending,
        stdout,
        stderr,
        outputs,
        hotpath,
    })
}

fn preflight_response(value: Value, what: &str) -> Result<PreflightResponse, String> {
    let mut fields = Fields::of(value, what)?;
    let start = match fields.take("start_error") {
        None => None,
        Some(value) => {
            let mut start = Fields::of(value, &format!("{what}.start_error"))?;
            let operation = match start.text("operation")?.as_deref() {
                Some("resolve") => "resolve",
                Some("execute") => "execute",
                _ => {
                    return Err(format!(
                        "{what}.start_error.operation is \"resolve\" or \"execute\""
                    ));
                }
            };
            let message = start.text("message")?.unwrap_or_default();
            start.done()?;
            Some(PreflightEnding::StartError { operation, message })
        }
    };
    let ends = has(&fields, "exit") || has(&fields, "signal");
    let ended = ending(&mut fields, false)?;
    let ending = match start {
        None => match ended {
            Ending::Exit(code) => PreflightEnding::Exit(code),
            Ending::Signal(signal) => PreflightEnding::Signal(signal),
            Ending::StoppedAt(_) => unreachable!("a preflight response reads no stopped_at"),
        },
        Some(start) if !ends => start,
        Some(_) => {
            return Err(format!(
                "{what} gives start_error and an ending: a program that never started has none"
            ));
        }
    };
    let stdout = fields.bytes("stdout")?;
    let stderr = fields.bytes("stderr")?;
    if matches!(ending, PreflightEnding::StartError { .. })
        && (!stdout.is_empty() || !stderr.is_empty())
    {
        return Err(format!(
            "{what} gives start_error and output: a program that never started wrote none"
        ));
    }
    fields.done()?;
    Ok(PreflightResponse {
        ending,
        stdout,
        stderr,
    })
}

fn has(fields: &Fields, key: &str) -> bool {
    fields.fields.iter().any(|(k, _)| k == key)
}

/// A fixture value as TOML: a table a table, a list an array, an integral
/// number within 2^53 an integer and any other number a float.
fn toml_value(value: &Value, what: &str) -> Result<toml::Value, String> {
    Ok(match value {
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Text(text) => toml::Value::String(text.clone()),
        Value::Number(n) if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_992.0 => {
            #[allow(clippy::cast_possible_truncation)] // Integral and within 2^53.
            toml::Value::Integer(*n as i64)
        }
        Value::Number(n) => toml::Value::Float(*n),
        Value::List(items) => toml::Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, item)| toml_value(item, &format!("{what}[{}]", i + 1)))
                .collect::<Result<_, _>>()?,
        ),
        Value::Table(fields) => toml::Value::Table(
            fields
                .iter()
                .map(|(key, item)| Ok((key.clone(), toml_value(item, &format!("{what}.{key}"))?)))
                .collect::<Result<_, String>>()?,
        ),
        Value::Null | Value::Handle(_) => {
            return Err(format!(
                "{what} holds a {}, which TOML has no value for",
                shape(value)
            ));
        }
    })
}
