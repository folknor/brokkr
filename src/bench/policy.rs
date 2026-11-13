//! The call policy every host applies before anything runs: what a script's
//! `binary()` and `binary_with()` declare, judged once here and used by the
//! real hosts (`runner::host`, `runner::preflight`) and the virtual ones a
//! contract test runs (`super::contract`), so the two cannot disagree about
//! which calls a script may make.
//!
//! What is judged here is deterministic state, never the filesystem: the
//! handle table and its lookups, the measured-once rule, the declared access
//! (`resources::call_access`), the page-cache options' handle rules, and a
//! preflight's refusals. What needs the disk stays with the real host:
//! admission against other leases, program resolution, inode identity.
//!
//! One rule is judged after a call: `Declared::ended`, the postcondition a
//! `stop_at` call holds its child to, on the ending either host reports.

use std::path::PathBuf;
use std::time::Duration;

use super::lua::{Abort, CallOptions, CallOutcome, HandleRef, Word};
use super::resources::{self, Handle};
use super::store::CacheMode;
use crate::reservations::Access;

/// Every handle an iteration gave out, by the index the script's table
/// carries: the inputs first, in the order the args number them.
#[derive(Debug, Default)]
pub(crate) struct Handles {
    given: Vec<Handle>,
}

impl Handles {
    pub(crate) fn new(inputs: Vec<Handle>) -> Self {
        Self { given: inputs }
    }

    /// Give `handle` out, refused when its path is not UTF-8, which a
    /// script cannot pass on.
    pub(crate) fn give(&mut self, handle: Handle) -> Result<HandleRef, Abort> {
        let path = handle
            .path()
            .to_str()
            .ok_or_else(|| {
                Abort::Failed(format!(
                    "{} is not UTF-8, which a script cannot pass on",
                    handle.path().display()
                ))
            })?
            .to_owned();
        self.given.push(handle);
        Ok(HandleRef {
            index: self.given.len() - 1,
            path,
        })
    }

    /// The handle the script's table carries `index` of.
    pub(crate) fn get(&self, index: usize, function: &str) -> Result<Handle, Abort> {
        self.given
            .get(index)
            .cloned()
            .ok_or_else(|| Abort::Failed(format!("{function}: handle {index} was never given out")))
    }
}

/// One `binary()` call of `bench()` as policy admitted it: the argv the
/// child gets, every handle as listed, and the access the call declares.
#[derive(Debug, Clone)]
pub(crate) struct Declared {
    /// The argv words as the child sees them, a handle as its path.
    pub(crate) words: Vec<String>,
    /// For each argv word, the handle it came from, if one did.
    pub(crate) word_handles: Vec<Option<Handle>>,
    pub(crate) reads: Vec<Handle>,
    pub(crate) writes: Vec<Handle>,
    pub(crate) stdin: Option<Handle>,
    pub(crate) env: Vec<(String, String)>,
    /// The stage a `hotpath` call is marked with.
    pub(crate) hotpath: Option<String>,
    /// The exact marker that ends the child.
    pub(crate) stop_at: Option<String>,
    /// The external program the call runs in place of the snapshot.
    pub(crate) program: Option<String>,
    /// The call whose child's wall is the iteration's result.
    pub(crate) measured: bool,
    /// Inputs prepared before launch, each with its mode, as listed.
    pub(crate) cache: Vec<(CacheMode, Handle)>,
    /// The most the child may live, from its launch.
    pub(crate) deadline: Option<Duration>,
    /// What the call declares of each path: what admission is asked for.
    pub(crate) access: Vec<(PathBuf, Access)>,
}

impl Declared {
    /// The call's postcondition on how its child ended: a call naming
    /// `stop_at` declares a required measurement boundary, so its child must
    /// have been stopped there. Ending any other way, an exit of any code or
    /// a signal, fails the iteration, since the run went past the phase the
    /// call measures; a renamed marker never turns a phase run into a full
    /// one. What is judged is whether the requested stop fired, never
    /// whether the marker text appears in the evidence: a marker a
    /// descendant writes after the child exited is drained into the
    /// evidence without stopping anything. An interrupted call never gets
    /// here. A stop that may or may not fire is not offered; it would be an
    /// option of its own.
    pub(crate) fn ended(&self, program: &str, outcome: &CallOutcome) -> Result<(), Abort> {
        let Some(marker) = &self.stop_at else {
            return Ok(());
        };
        if outcome.stopped_at.as_deref() == Some(marker.as_str()) {
            return Ok(());
        }
        let how = match (outcome.exit, outcome.signal, &outcome.stopped_at) {
            (Some(code), _, _) => format!("exited {code}"),
            (None, Some(signal), _) => format!("was killed by signal {signal}"),
            (None, None, Some(other)) => format!("stopped at {other}"),
            (None, None, None) => "ended".to_owned(),
        };
        Err(Abort::Failed(format!(
            "{program} {how} before its stop_at marker {marker} stopped it: the call \
             measures up to {marker}, and the run went past it"
        )))
    }
}

/// The calls of one iteration: its handles and whether a call was marked
/// measured yet.
#[derive(Debug, Default)]
pub(crate) struct Calls {
    pub(crate) handles: Handles,
    measured: bool,
}

impl Calls {
    pub(crate) fn new(inputs: Vec<Handle>) -> Self {
        Self {
            handles: Handles::new(inputs),
            measured: false,
        }
    }

    /// Judge one call of `bench()`. A call marked measured claims the
    /// iteration's one measured slot here, as it is admitted, so a second is
    /// refused whatever became of the first.
    #[allow(clippy::too_many_lines)] // Every rule of one call, in the order a refusal is named.
    pub(crate) fn declare(
        &mut self,
        words: Vec<Word>,
        options: CallOptions,
    ) -> Result<Declared, Abort> {
        if options.measured && self.measured {
            return Err(Abort::Failed(
                "binary_with(): a second call marked measured: an iteration's result is one \
                 child's wall"
                    .into(),
            ));
        }
        let handles = &self.handles;
        let mut cache = Vec::new();
        for (mode, list) in [
            (CacheMode::Cold, &options.cold),
            (CacheMode::Warm, &options.warm),
        ] {
            for index in list {
                let what = format!("binary_with(): {}", mode.name());
                cache.push((mode, handles.get(*index, &what)?));
            }
        }
        let mut text = Vec::with_capacity(words.len());
        let mut word_handles = Vec::with_capacity(words.len());
        let mut argv = Vec::new();
        for word in words {
            match word {
                Word::Text(word) => {
                    text.push(word);
                    word_handles.push(None);
                }
                Word::Handle(index) => {
                    let handle = handles.get(index, "binary()")?;
                    text.push(handle.path().display().to_string());
                    word_handles.push(Some(handle.clone()));
                    argv.push(handle);
                }
            }
        }
        // A NUL is the script's to answer for, as a preflight's is: no
        // child could be given it.
        if text.iter().any(|word| word.contains('\0')) {
            return Err(Abort::Failed(
                "binary(): an argument holds a NUL byte, which no child can be given".into(),
            ));
        }
        let reads = options
            .reads
            .iter()
            .map(|index| handles.get(*index, "binary_with(): reads"))
            .collect::<Result<Vec<_>, _>>()?;
        let writes = options
            .writes
            .iter()
            .map(|index| handles.get(*index, "binary_with(): writes"))
            .collect::<Result<Vec<_>, _>>()?;
        let stdin = options
            .stdin
            .map(|index| handles.get(index, "binary_with(): stdin"))
            .transpose()?;
        let (argv_refs, read_refs, write_refs): (Vec<&Handle>, Vec<&Handle>, Vec<&Handle>) = (
            argv.iter().collect(),
            reads.iter().collect(),
            writes.iter().collect(),
        );
        let access = resources::call_access(&argv_refs, &read_refs, &write_refs, stdin.as_ref())
            .map_err(|e| Abort::Failed(format!("the call was refused: {e}")))?;
        for (at, (mode, handle)) in cache.iter().enumerate() {
            let shown = handle.path().display().to_string();
            let what = format!("binary_with(): {} {shown}", mode.name());
            handle
                .preparable()
                .map_err(|e| Abort::Failed(format!("{what}: {e}")))?;
            let passed =
                argv.contains(handle) || reads.contains(handle) || stdin.as_ref() == Some(handle);
            if !passed {
                return Err(Abort::Failed(format!(
                    "{what}: the call does not pass it (argv, reads or stdin): only a file the \
                     child is given is prepared"
                )));
            }
            // Named at its later listing, as the real host names an inode
            // listed in both modes.
            if let Some((other, _)) = cache[..at]
                .iter()
                .find(|(m, h)| m != mode && h.path() == handle.path())
            {
                return Err(Abort::Failed(format!(
                    "{what}: the same file is also listed {} ({shown})",
                    other.name()
                )));
            }
        }
        if options.measured {
            self.measured = true;
        }
        Ok(Declared {
            words: text,
            word_handles,
            reads,
            writes,
            stdin,
            env: options.env,
            hotpath: options.hotpath,
            stop_at: options.stop_at,
            program: options.program,
            measured: options.measured,
            cache,
            deadline: options.deadline,
            access,
        })
    }
}

/// One call of `preflight()` as policy admitted it: an external program
/// with text arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreflightCall {
    /// The program as the script named it.
    pub(crate) program: String,
    pub(crate) argv: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    /// The most the child may live, from its launch: a probe that hangs is
    /// bounded as a bench's child is.
    pub(crate) deadline: Option<Duration>,
}

/// Judge one call of `preflight()`: an external program only, no handle, no
/// measurement, no NUL in an argument.
pub(crate) fn declare_preflight(
    words: Vec<Word>,
    options: CallOptions,
) -> Result<PreflightCall, Abort> {
    let Some(program) = options.program else {
        return Err(Abort::Failed(
            "binary() runs the build, which does not exist yet inside preflight()".into(),
        ));
    };
    let refused = |what: &str| {
        Abort::Failed(format!(
            "binary_with(): {what} is not available inside preflight(), which holds no handle \
             and measures nothing"
        ))
    };
    if !options.reads.is_empty() || !options.writes.is_empty() || options.stdin.is_some() {
        return Err(refused("a handle"));
    }
    if options.hotpath.is_some() {
        return Err(refused("hotpath"));
    }
    if options.stop_at.is_some() {
        return Err(refused("stop_at"));
    }
    if options.measured {
        return Err(refused("measured"));
    }
    if !options.cold.is_empty() || !options.warm.is_empty() {
        return Err(refused("cold and warm"));
    }
    let mut argv = Vec::with_capacity(words.len());
    for word in words {
        match word {
            Word::Text(text) => argv.push(text),
            Word::Handle(_) => return Err(refused("a handle")),
        }
    }
    // A NUL is the script's to answer for, never a failure of brokkr's
    // spawn, which would refuse it the same way.
    if argv.iter().any(|a| a.contains('\0')) {
        return Err(Abort::Failed(format!(
            "binary_with(): an argument to {program} holds a NUL byte"
        )));
    }
    Ok(PreflightCall {
        program,
        argv,
        env: options.env,
        deadline: options.deadline,
    })
}

/// Refused unless `path` is one of the arm's bound `inputs`, the only paths
/// `page_cache()` takes.
pub(crate) fn page_cache_input(inputs: &[PathBuf], path: &str) -> Result<(), Abort> {
    if inputs.iter().any(|input| input.as_os_str() == path) {
        return Ok(());
    }
    Err(Abort::Failed(format!(
        "page_cache(): {path} is not a bound input of this arm: only an input's path, as \
         params gives it, is checked"
    )))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::bench::resources::Origin;

    fn calls() -> Calls {
        let mut calls = Calls::new(vec![
            Handle::new(PathBuf::from("/d/a"), Origin::Input),
            Handle::new(PathBuf::from("/d/a"), Origin::Input),
        ]);
        calls
            .handles
            .give(Handle::new(PathBuf::from("/i/out/o"), Origin::Output))
            .unwrap();
        calls
    }

    fn refusal(calls: &mut Calls, words: Vec<Word>, options: CallOptions) -> String {
        calls.declare(words, options).unwrap_err().to_string()
    }

    /// The rules both hosts share: measured once per iteration, cache
    /// options only on inputs the call passes, never one file in both
    /// modes, and the declared access is the one admission is asked for.
    #[test]
    fn a_call_is_judged_once_for_every_host() {
        let mut calls = calls();
        let declared = calls
            .declare(
                vec![Word::Text("x".into()), Word::Handle(0), Word::Handle(2)],
                CallOptions {
                    measured: true,
                    cold: vec![0],
                    ..CallOptions::default()
                },
            )
            .unwrap();
        assert_eq!(declared.words, ["x", "/d/a", "/i/out/o"]);
        assert_eq!(
            declared.access,
            [
                (PathBuf::from("/d/a"), Access::Read),
                (PathBuf::from("/i/out/o"), Access::Write)
            ]
        );
        assert!(
            refusal(
                &mut calls,
                Vec::new(),
                CallOptions {
                    measured: true,
                    ..CallOptions::default()
                }
            )
            .contains("a second call marked measured")
        );
        let mut calls = super::tests::calls();
        assert!(
            refusal(
                &mut calls,
                Vec::new(),
                CallOptions {
                    cold: vec![0],
                    ..CallOptions::default()
                }
            )
            .contains("the call does not pass it")
        );
        assert!(
            refusal(
                &mut calls,
                vec![Word::Handle(2)],
                CallOptions {
                    cold: vec![2],
                    ..CallOptions::default()
                }
            )
            .contains("only an input is prepared")
        );
        assert!(
            refusal(
                &mut calls,
                vec![Word::Handle(0), Word::Handle(1)],
                CallOptions {
                    cold: vec![0],
                    warm: vec![1],
                    ..CallOptions::default()
                }
            )
            .contains("also listed")
        );
        assert!(
            refusal(
                &mut calls,
                Vec::new(),
                CallOptions {
                    writes: vec![0],
                    ..CallOptions::default()
                }
            )
            .contains("read-only for the run")
        );
        assert!(
            refusal(&mut calls, vec![Word::Handle(9)], CallOptions::default())
                .contains("never given out")
        );
        assert!(
            refusal(
                &mut calls,
                vec![Word::Text("a\0b".into())],
                CallOptions::default()
            )
            .contains("NUL")
        );
        assert!(
            declare_preflight(
                vec![Word::Text("a\0b".into())],
                CallOptions {
                    program: Some("p".into()),
                    ..CallOptions::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("NUL")
        );
    }
}
