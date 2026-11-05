//! Handles: how a script bench's paths reach its children, and the access
//! each call declares for them (`reference/bench-resources.md`).
//!
//! A handle is a path with an origin. Inputs (the bound file-valued params)
//! are read; outputs and scratch, made by
//! the iteration, are written; a spool, a finished call's stdout, is read.
//! A handle passed to a call gets its origin's access by default; the call's
//! `reads` and `writes` lists override that and also declare handles the
//! argv does not show (a path inside a file the script wrote, a
//! `--output=PATH` spelling), and its `stdin` is read. A handle in both
//! lists is refused, and so is an input or a spool in `writes`, and stdin in
//! `writes`: inputs and spools are read-only for the run. The iteration
//! (`crate::reservations::Iteration`) admits the result.
//! Handles guarantee that what they name is reserved; they do not confine a
//! child, and a literal string the child treats as a path is undeclared.
//!
//! brokkr's own file builtins act on handles too, each admitted as a call
//! would be: `read()` and `filesize()` take a file's handle (an input, an
//! output or a spool), `write()` an output's alone; a scratch handle names a
//! directory, which none of them takes.

use std::collections::BTreeMap;
use std::io::Read as _;
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

impl Origin {
    fn named(self) -> &'static str {
        match self {
            Self::Input => "an input",
            Self::Output => "an output",
            Self::Scratch => "a scratch directory",
            Self::Spool => "a spool",
        }
    }
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

    /// The access a call that passes this handle gets unless its lists say
    /// otherwise.
    pub(crate) fn default_access(&self) -> Access {
        match self.origin {
            Origin::Input | Origin::Spool => Access::Read,
            Origin::Output | Origin::Scratch => Access::Write,
        }
    }

    /// Whether a call may declare it written.
    pub(crate) fn writable(&self) -> bool {
        matches!(self.origin, Origin::Output | Origin::Scratch)
    }

    /// Whether it names a file brokkr itself may open (`read()`,
    /// `filesize()`, a call's stdin): every origin but scratch.
    fn is_file(&self) -> bool {
        self.origin != Origin::Scratch
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

    /// The inputs as handles, in the order `open` took them.
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
    /// `writes` overriding and adding, and `stdin` read; with `report`, an
    /// empty hotpath report made for it (`Call::report`).
    pub(crate) fn call(
        &mut self,
        program: &Path,
        argv: &[&Handle],
        reads: &[&Handle],
        writes: &[&Handle],
        stdin: Option<&Handle>,
        report: bool,
    ) -> Result<Call, DevError> {
        let declared = call_access(argv, reads, writes, stdin)?;
        self.iteration
            .call(program, &declared, stdin.map(Handle::path), report)
    }

    /// A finished call's hotpath report, `None` when it holds more than
    /// `limit` bytes (counted as read).
    pub(crate) fn read_report(
        &mut self,
        path: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, DevError> {
        let file = self.iteration.open_file(path)?;
        let mut bytes = Vec::new();
        let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
        file.take(cap)
            .read_to_end(&mut bytes)
            .map_err(|e| DevError::Config(format!("cannot read {}: {e}", path.display())))?;
        Ok((bytes.len() <= limit).then_some(bytes))
    }

    /// The whole file `handle` names, refused past `limit` bytes (counted
    /// as read, not as the file says).
    pub(crate) fn read(&mut self, handle: &Handle, limit: usize) -> Result<Vec<u8>, DevError> {
        let file = self.open_file(handle)?;
        let mut bytes = Vec::new();
        let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
        file.take(cap)
            .read_to_end(&mut bytes)
            .map_err(|e| DevError::Config(format!("cannot read {}: {e}", handle.path.display())))?;
        if bytes.len() > limit {
            return Err(DevError::Config(format!(
                "{} holds more than {limit} bytes, the most a script's string holds",
                handle.path.display()
            )));
        }
        Ok(bytes)
    }

    /// The size of the file `handle` names, from its open descriptor.
    pub(crate) fn filesize(&mut self, handle: &Handle) -> Result<u64, DevError> {
        let file = self.open_file(handle)?;
        Ok(file.metadata()?.len())
    }

    fn open_file(&mut self, handle: &Handle) -> Result<std::fs::File, DevError> {
        if !handle.is_file() {
            return Err(DevError::Config(format!(
                "{} is {}, not a file",
                handle.path.display(),
                handle.origin.named()
            )));
        }
        self.iteration.open_file(&handle.path)
    }

    /// Replace the file an output handle names with `contents`.
    pub(crate) fn write(&mut self, handle: &Handle, contents: &[u8]) -> Result<(), DevError> {
        if handle.origin != Origin::Output {
            let why = match handle.origin {
                Origin::Input | Origin::Spool => "read-only for the run",
                _ => "a directory",
            };
            return Err(DevError::Config(format!(
                "{} is {}, which is {why}: write() writes an output's file",
                handle.path.display(),
                handle.origin.named()
            )));
        }
        self.iteration.write_file(&handle.path, contents)
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
/// `reads` and `writes`, which may also add handles, and by `stdin`, read.
/// Every refusal is decided on the lists as given, before they collapse into
/// one access per path.
pub(crate) fn call_access(
    argv: &[&Handle],
    reads: &[&Handle],
    writes: &[&Handle],
    stdin: Option<&Handle>,
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
            fixed.origin.named()
        )));
    }
    if let Some(stdin) = stdin {
        if writes.contains(&stdin) {
            return Err(DevError::Config(format!(
                "{} is the call's stdin, which is read: it cannot be in writes",
                stdin.path.display()
            )));
        }
        if !stdin.is_file() {
            return Err(DevError::Config(format!(
                "stdin {} is {}, not a file",
                stdin.path.display(),
                stdin.origin.named()
            )));
        }
    }
    let mut access: BTreeMap<&Path, Access> = BTreeMap::new();
    for handle in argv {
        access.insert(&handle.path, handle.default_access());
    }
    for handle in reads.iter().chain(stdin.as_ref()) {
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
        let declared = call_access(&[&input, &out, &spool], &[], &[], None).unwrap();
        assert_eq!(
            declared,
            vec![
                (PathBuf::from("/d/in"), Access::Read),
                (PathBuf::from("/i/out/o"), Access::Write),
                (PathBuf::from("/i/stdout/0"), Access::Read),
            ]
        );
        let declared = call_access(&[&out], &[&out], &[&scratch], None).unwrap();
        assert_eq!(
            declared,
            vec![
                (PathBuf::from("/i/out/o"), Access::Read),
                (PathBuf::from("/i/scratch/s"), Access::Write),
            ],
            "reads overrides the default, writes adds an unseen handle"
        );
        let declared = call_access(&[&out], &[&out], &[], Some(&out)).unwrap();
        assert_eq!(
            declared,
            vec![(PathBuf::from("/i/out/o"), Access::Read)],
            "stdin is read, over an argv output's default, and a read repeats it harmlessly"
        );
    }

    #[test]
    fn both_lists_and_read_only_writes_are_refused() {
        let input = handle("/d/in", Origin::Input);
        let out = handle("/i/out/o", Origin::Output);
        let spool = handle("/i/stdout/0", Origin::Spool);
        let scratch = handle("/i/scratch/s", Origin::Scratch);
        assert!(call_access(&[], &[&out], &[&out], None).is_err());
        assert!(call_access(&[], &[], &[&input], None).is_err());
        assert!(call_access(&[], &[], &[&spool], None).is_err());
        assert!(call_access(&[], &[], &[&out], Some(&out)).is_err());
        assert!(call_access(&[], &[], &[], Some(&scratch)).is_err());
        assert!(!input.writable() && !spool.writable() && out.writable());
    }

    /// The handle layer over a real iteration: an output written by one
    /// call, its spool and output fed to the next as reads, and brokkr's own
    /// reads and writes.
    #[test]
    fn handles_drive_an_iteration() {
        let _lock = crate::test_scratch::process_global_lock();
        let root = crate::test_scratch::scratch("bench_resources", "drive");
        let input = root.join("in.txt");
        std::fs::write(&input, b"x").unwrap();
        let mut resources = Resources::open(&root, std::slice::from_ref(&input)).unwrap();
        let input = resources.inputs()[0].clone();
        assert_eq!(input.origin, Origin::Input);
        let out = resources.output("o").unwrap();
        let scratch = resources.scratch("s").unwrap();
        assert!(scratch.path().is_dir() && !out.path().exists());
        assert_eq!(scratch.default_access(), Access::Write);
        let sh = Path::new("/bin/true");
        let spool = resources
            .call(sh, &[&input, &out], &[], &[], None, false)
            .unwrap()
            .finish()
            .unwrap();
        let spool = spool_handle(&spool);
        assert!(resources.call(sh, &[], &[], &[&spool], None, false).is_err());
        resources
            .call(sh, &[&spool], &[&out], &[], None, false)
            .unwrap()
            .finish()
            .unwrap();

        assert_eq!(resources.read(&input, 16).unwrap(), b"x");
        assert!(
            resources
                .read(&input, 0)
                .unwrap_err()
                .to_string()
                .contains("more than 0")
        );
        resources.write(&out, b"written").unwrap();
        resources.write(&out, b"again").unwrap();
        assert_eq!(resources.read(&out, 16).unwrap(), b"again");
        assert_eq!(resources.filesize(&out).unwrap(), 5);
        assert_eq!(resources.filesize(&spool).unwrap(), 0);
        for refused in [&input, &spool, &scratch] {
            assert!(resources.write(refused, b"no").is_err(), "{refused:?}");
        }
        assert!(resources.read(&scratch, 16).is_err());
        assert_eq!(std::fs::read(input.path()).unwrap(), b"x");
        resources.release().unwrap();
    }
}
