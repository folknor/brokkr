//! The seam to the linked dellingr crate, the Lua VM script benches run on.
//!
//! brokkr has a module of its own named `dellingr` (the `brokkr dellingr`
//! command), so the crate is reached only here, by its absolute `::dellingr`
//! path; a textlint rule keeps every other file off it, and a change in the
//! crate's API touches this file alone.

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
    match ::dellingr::Program::compile(source, None) {
        Ok(_) => Ok(()),
        Err(error) => Err(CompileError {
            line: error.line_num(),
            message: error.kind().to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_valid_chunk_compiles_and_runs_nothing() {
        // `error()` at top level would fail if the chunk ran.
        assert_eq!(
            compile(b"function bench()\n  return nil\nend\nerror('ran')\n"),
            Ok(())
        );
    }

    #[test]
    fn a_syntax_error_names_its_line() {
        let err = compile(b"function bench()\n  return (\nend\n").unwrap_err();
        assert_eq!(err.line, 3, "{err}");
        assert!(!err.message.is_empty());
    }

    #[test]
    fn a_misspelt_builtin_still_compiles() {
        assert_eq!(
            compile(b"function bench() return binray('x') end\n"),
            Ok(())
        );
    }
}
