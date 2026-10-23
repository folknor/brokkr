//! Render subprocess argument vectors for measured rows, and the resolved
//! invocation every measured spawn executes.

use std::path::Path;

/// A program together with the arguments resolved for it - the only thing
/// the measure runner executes.
///
/// The program is borrowed from whatever resolved the arguments (the runner's
/// built binary, or a project handle such as
/// [`PbfhoggBinary`](crate::pbfhogg::binary::PbfhoggBinary) that negotiated
/// flags against that executable), so an invocation cannot outlive the
/// resolution it came from, and the program a row records is the program that
/// ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invocation<'a> {
    program: &'a Path,
    args: Vec<String>,
}

impl<'a> Invocation<'a> {
    pub(crate) fn new(program: &'a Path, args: Vec<String>) -> Self {
        Self { program, args }
    }

    pub(crate) fn program(&self) -> &'a Path {
        self.program
    }

    /// The program as the string spawn helpers and error messages take.
    pub(crate) fn program_string(&self) -> String {
        self.program.display().to_string()
    }

    #[cfg(test)]
    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }

    pub(crate) fn arg_refs(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }

    /// The `cli_args` column value for this invocation.
    pub(crate) fn cli_args(&self) -> String {
        format_cli_args(&self.program_string(), &self.arg_refs())
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
