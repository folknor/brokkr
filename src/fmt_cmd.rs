//! Cargo formatting invocation.
use crate::{error::DevError, lockfile};

pub(crate) fn cmd_fmt(args: &[String], lock: Option<&lockfile::LockGuard>) -> Result<(), DevError> {
    let mut cargo_args: Vec<String> = vec!["fmt".into()];
    cargo_args.extend(args.iter().cloned());
    match crate::runnables::run_cargo(&cargo_args, lock)? {
        0 => Ok(()),
        code => Err(DevError::ExitCode(code)),
    }
}
