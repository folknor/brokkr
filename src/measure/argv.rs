//! Render subprocess argument vectors for measured rows, and the resolved
//! invocation every measured spawn executes.

use std::path::Path;

/// A program together with the arguments resolved for it - the only thing
/// the measured spawn paths execute.
///
/// The program is borrowed from whatever resolved the arguments, so an
/// invocation cannot outlive the resolution it came from, and the program a row records
/// is the program that ran. Nothing path-bearing is readable outside
/// `crate::measure`: callers spawn through the harness.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Invocation<'a> {
    program: &'a Path,
    args: Vec<String>,
}

impl<'a> Invocation<'a> {
    pub(crate) fn new(program: &'a Path, args: Vec<String>) -> Self {
        Self { program, args }
    }

    /// The `cli_args` column value for this invocation.
    pub(crate) fn cli_args(&self) -> String {
        format_cli_args(&self.program_string(), &self.arg_refs())
    }

    pub(in crate::measure) fn program(&self) -> &'a Path {
        self.program
    }

    pub(in crate::measure) fn program_string(&self) -> String {
        self.program.display().to_string()
    }

    pub(in crate::measure) fn arg_refs(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }
}

impl std::fmt::Debug for Invocation<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invocation")
            .field("args", &self.args)
            .finish_non_exhaustive()
    }
}

/// Format a program path and argument slice into a single command-line string.
///
/// Quotes arguments that contain spaces. Used to populate `BenchConfig.cli_args`.
pub fn format_cli_args(program: &str, args: &[&str]) -> String {
    let mut parts = Vec::with_capacity(1 + args.len());
    parts.push(maybe_quote(program));
    for arg in args {
        parts.push(maybe_quote(arg));
    }
    parts.join(" ")
}

pub(super) fn maybe_quote(s: &str) -> String {
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s.to_owned()
    }
}
