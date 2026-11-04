//! Handles: how a script bench's paths reach its children, and the access
//! each call declares for them (`reference/bench-resources.md`).
//!
//! A handle is a path with an origin. Inputs (the resolved config's file
//! entries and file-valued params) are read; outputs and scratch, made by
//! the iteration, are written; a spool, a finished call's stdout, is read.
//! A handle passed to a call gets its origin's access by default; the call's
//! `reads` and `writes` lists override that and also declare handles the
//! argv does not show (a path inside a file the script wrote, a
//! `--output=PATH` spelling). A handle in both lists is refused, and so is
//! an input or a spool in `writes`: both are read-only for the run. The
//! iteration (`crate::reservations::Iteration`) admits the result.
//! Handles guarantee that what they name is reserved; they do not confine a
//! child, and a literal string the child treats as a path is undeclared.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::reservations::{Access, Call, Iteration, Spool};

/// Where a handle came from, which decides its default access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Input,
    Output,
    Scratch,
    Spool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handle {
    path: PathBuf,
    origin: Origin,
}

impl Handle {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn origin(&self) -> Origin {
        self.origin
    }

    /// The access a call that passes this handle gets unless its lists say
    /// otherwise.
    pub(crate) fn default_access(&self) -> Access {
        match self.origin {
            Origin::Input | Origin::Spool => Access::Read,
            Origin::Output | Origin::Scratch => Access::Write,
        }
    }

    /// Whether a call may declare it written, and `write()` may write it.
    pub(crate) fn writable(&self) -> bool {
        matches!(self.origin, Origin::Output | Origin::Scratch)
    }
}

/// One iteration's resources as handles.
#[derive(Debug)]
pub(crate) struct Resources {
    iteration: Iteration,
    inputs: Vec<Handle>,
}

impl Resources {
    /// Admit an iteration reading `inputs`, its directory in `location`.
    pub(crate) fn open(location: &Path, inputs: &[PathBuf]) -> Result<Self, DevError> {
        Ok(Self {
            iteration: Iteration::admit(location, inputs)?,
            inputs: inputs
                .iter()
                .map(|path| Handle {
                    path: path.clone(),
                    origin: Origin::Input,
                })
                .collect(),
        })
    }

    pub(crate) fn inputs(&self) -> &[Handle] {
        &self.inputs
    }

    pub(crate) fn output(&mut self, name: &str) -> Result<Handle, DevError> {
        Ok(Handle {
            path: self.iteration.output(name)?,
            origin: Origin::Output,
        })
    }

    pub(crate) fn scratch(&mut self, name: &str) -> Result<Handle, DevError> {
        Ok(Handle {
            path: self.iteration.scratch(name)?,
            origin: Origin::Scratch,
        })
    }

    /// Admit one call of `program` passing `argv` handles, with `reads` and
    /// `writes` overriding and adding.
    pub(crate) fn call(
        &mut self,
        program: &Path,
        argv: &[&Handle],
        reads: &[&Handle],
        writes: &[&Handle],
    ) -> Result<Call, DevError> {
        let declared = call_access(argv, reads, writes)?;
        self.iteration.call(program, &declared)
    }

    pub(crate) fn release(self) -> Result<(), DevError> {
        self.iteration.release()
    }
}

/// A finished call's spool as a handle, for feeding it onward.
pub(crate) fn spool_handle(spool: &Spool) -> Handle {
    Handle {
        path: spool.path().to_path_buf(),
        origin: Origin::Spool,
    }
}

/// The access one call declares: each `argv` handle's default, overridden by
/// `reads` and `writes`, which may also add handles.
pub(crate) fn call_access(
    argv: &[&Handle],
    reads: &[&Handle],
    writes: &[&Handle],
) -> Result<Vec<(PathBuf, Access)>, DevError> {
    if let Some(both) = reads.iter().find(|r| writes.contains(r)) {
        return Err(DevError::Config(format!(
            "{} is in both reads and writes",
            both.path.display()
        )));
    }
    if let Some(fixed) = writes.iter().find(|w| !w.writable()) {
        return Err(DevError::Config(format!(
            "{} is {} and read-only for the run: it cannot be in writes",
            fixed.path.display(),
            match fixed.origin {
                Origin::Input => "an input",
                Origin::Spool => "a spool",
                Origin::Output | Origin::Scratch => "writable",
            }
        )));
    }
    let mut access: BTreeMap<&Path, Access> = BTreeMap::new();
    for handle in argv {
        access.insert(&handle.path, handle.default_access());
    }
    for handle in reads {
        access.insert(&handle.path, Access::Read);
    }
    for handle in writes {
        access.insert(&handle.path, Access::Write);
    }
    Ok(access
        .into_iter()
        .map(|(path, access)| (path.to_path_buf(), access))
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn handle(path: &str, origin: Origin) -> Handle {
        Handle {
            path: PathBuf::from(path),
            origin,
        }
    }

    #[test]
    fn a_handle_gets_its_origins_access_unless_a_list_overrides_it() {
        let input = handle("/d/in", Origin::Input);
        let out = handle("/i/out/o", Origin::Output);
        let scratch = handle("/i/scratch/s", Origin::Scratch);
        let spool = handle("/i/stdout/0", Origin::Spool);
        let declared = call_access(&[&input, &out, &spool], &[], &[]).unwrap();
        assert_eq!(
            declared,
            vec![
                (PathBuf::from("/d/in"), Access::Read),
                (PathBuf::from("/i/out/o"), Access::Write),
                (PathBuf::from("/i/stdout/0"), Access::Read),
            ]
        );
        let declared = call_access(&[&out], &[&out], &[&scratch]).unwrap();
        assert_eq!(
            declared,
            vec![
                (PathBuf::from("/i/out/o"), Access::Read),
                (PathBuf::from("/i/scratch/s"), Access::Write),
            ],
            "reads overrides the default, writes adds an unseen handle"
        );
    }

    #[test]
    fn both_lists_and_read_only_writes_are_refused() {
        let input = handle("/d/in", Origin::Input);
        let out = handle("/i/out/o", Origin::Output);
        let spool = handle("/i/stdout/0", Origin::Spool);
        assert!(call_access(&[], &[&out], &[&out]).is_err());
        assert!(call_access(&[], &[], &[&input]).is_err());
        assert!(call_access(&[], &[], &[&spool]).is_err());
        assert!(!input.writable() && !spool.writable() && out.writable());
    }

    /// The handle layer over a real iteration: an output written by one
    /// call, its spool and output fed to the next as reads.
    #[test]
    fn handles_drive_an_iteration() {
        let _lock = crate::test_scratch::process_global_lock();
        let root = crate::test_scratch::scratch("bench_resources", "drive");
        let input = root.join("in.txt");
        std::fs::write(&input, b"x").unwrap();
        let mut resources = Resources::open(&root, std::slice::from_ref(&input)).unwrap();
        let input = resources.inputs()[0].clone();
        assert_eq!(input.origin(), Origin::Input);
        let out = resources.output("o").unwrap();
        let scratch = resources.scratch("s").unwrap();
        assert!(scratch.path().is_dir() && !out.path().exists());
        assert_eq!(scratch.default_access(), Access::Write);
        let sh = Path::new("/bin/true");
        let spool = resources
            .call(sh, &[&input, &out], &[], &[])
            .unwrap()
            .finish()
            .unwrap();
        let spool = spool_handle(&spool);
        assert!(resources.call(sh, &[], &[], &[&spool]).is_err());
        resources
            .call(sh, &[&spool], &[&out], &[])
            .unwrap()
            .finish()
            .unwrap();
        resources.release().unwrap();
    }
}
