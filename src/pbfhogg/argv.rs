//! A pbfhogg argv together with what it uses, built by the same calls.
//!
//! Every pbfhogg invocation brokkr makes is a [`PbfArgv`]: path arguments go
//! in through methods that say what the path is - an input read, an output
//! written, a directory output replaced, an extract config with the outputs
//! it names - so the reservation a launch makes (`crate::reservations`) can
//! never disagree with the argv that runs. Arguments that name no path go in
//! through [`PbfArgv::arg`]. Scratch is declared from the actual argument
//! combination: pbfhogg makes run scratch for `add-locations-to-ways`,
//! `build-geocode-index`, `diff --format osc`, and every text `diff` but a
//! quiet one or one with an explicit `-j 1`, which refuse `--tmp-dir`.

use std::path::{Path, PathBuf};

use crate::error::DevError;

/// What an argv needs for pbfhogg to replace its outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Overwrite {
    /// Reads only.
    NoOutput,
    /// Writes a brokkr-owned output: pass `--overwrite` when the executable
    /// accepts it.
    Negotiate,
    /// Writes a brokkr-owned output, and a flag the argv already carries
    /// (`build-geocode-index --force`) replaces it in every pbfhogg version.
    ImpliedByForce,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PbfArgv {
    args: Vec<String>,
    reads: Vec<PathBuf>,
    writes: Vec<PathBuf>,
    /// Where run scratch goes when the argv names no output.
    scratch_root: Option<PathBuf>,
    forced: bool,
}

impl PbfArgv {
    pub(crate) fn new(subcommand: &str) -> Self {
        Self {
            args: vec![subcommand.to_owned()],
            reads: Vec::new(),
            writes: Vec::new(),
            scratch_root: None,
            forced: false,
        }
    }

    /// An argument naming no path.
    pub(crate) fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// [`arg`](Self::arg) when `cond` holds.
    pub(crate) fn arg_if(self, cond: bool, arg: impl Into<String>) -> Self {
        if cond { self.arg(arg) } else { self }
    }

    /// Every argument of `args`, none naming a path.
    pub(crate) fn args_from<S: Into<String>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// A positional input path.
    pub(crate) fn input(mut self, path: impl AsRef<Path>) -> Result<Self, DevError> {
        let path = path.as_ref();
        self.args.push(crate::utf8_path::path_string(path)?);
        self.reads.push(path.to_path_buf());
        Ok(self)
    }

    /// `flag PATH` where the path is read.
    pub(crate) fn read_opt(mut self, flag: &str, path: impl AsRef<Path>) -> Result<Self, DevError> {
        let path = path.as_ref();
        self.args.push(flag.to_owned());
        self.args.push(crate::utf8_path::path_string(path)?);
        self.reads.push(path.to_path_buf());
        Ok(self)
    }

    /// `-o PATH`, a file or directory output.
    pub(crate) fn output(mut self, path: impl AsRef<Path>) -> Result<Self, DevError> {
        let path = path.as_ref();
        self.args.push("-o".into());
        self.args.push(crate::utf8_path::path_string(path)?);
        self.writes.push(path.to_path_buf());
        Ok(self)
    }

    /// `flag DIR`, a directory output publication replaces whole
    /// (`build-geocode-index --output-dir`).
    pub(crate) fn output_dir(mut self, flag: &str, path: impl AsRef<Path>) -> Result<Self, DevError> {
        let path = path.as_ref();
        self.args.push(flag.to_owned());
        self.args.push(crate::utf8_path::path_string(path)?);
        self.writes.push(path.to_path_buf());
        Ok(self)
    }

    /// `--config FILE` for `extract`, with every output the config names.
    pub(crate) fn extract_config(
        mut self,
        config: impl AsRef<Path>,
        outputs: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, DevError> {
        self = self.read_opt("--config", config)?;
        self.writes.extend(outputs);
        Ok(self)
    }

    /// The argv writes run scratch, in `root` when it names no output.
    pub(crate) fn scratch(mut self, root: impl AsRef<Path>) -> Self {
        self.scratch_root = Some(root.as_ref().to_path_buf());
        self
    }

    /// The argv carries a flag that replaces its outputs on its own.
    pub(crate) fn forces_overwrite(mut self) -> Self {
        self.forced = true;
        self
    }

    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }

    pub(crate) fn overwrite(&self) -> Overwrite {
        match (self.writes.is_empty(), self.forced) {
            (true, _) => Overwrite::NoOutput,
            (false, true) => Overwrite::ImpliedByForce,
            (false, false) => Overwrite::Negotiate,
        }
    }

    pub(crate) fn negotiates_overwrite(&self) -> bool {
        self.overwrite() == Overwrite::Negotiate
    }

    pub(crate) fn writes_output(&self) -> bool {
        !self.writes.is_empty()
    }

    pub(crate) fn writes_scratch(&self) -> bool {
        self.scratch_root.is_some()
    }

    pub(crate) fn reads(&self) -> &[PathBuf] {
        &self.reads
    }

    pub(crate) fn writes(&self) -> &[PathBuf] {
        &self.writes
    }

    pub(crate) fn scratch_root(&self) -> Option<&Path> {
        self.scratch_root.as_deref()
    }

    pub(crate) fn into_args(self) -> Vec<String> {
        self.args
    }
}

/// For observation (dry run prints it); running it goes through
/// [`PbfhoggBinary::invocation`](crate::pbfhogg::binary::PbfhoggBinary::invocation).
impl AsRef<[String]> for PbfArgv {
    fn as_ref(&self) -> &[String] {
        &self.args
    }
}

/// Whether a text `diff` with these settings makes a run scratch directory.
/// pbfhogg makes one at startup for every text diff except a quiet one and
/// one with an explicit `-j 1` - the two that also refuse `--tmp-dir` -
/// whatever the automatic job count resolves to and whether or not it is
/// verbose (`command_paths.rs` in pbfhogg's CLI; its prose reference also
/// excuses `-v`, which the implementation does not).
pub(crate) fn text_diff_writes_scratch(jobs: Option<usize>, quiet: bool) -> bool {
    !quiet && jobs != Some(1)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn paths_land_in_the_argv_and_the_effects_together() {
        let argv = PbfArgv::new("sort")
            .input("/d/in.pbf")
            .unwrap()
            .output("/s/out.pbf")
            .unwrap()
            .arg("-j")
            .arg("4");
        assert_eq!(argv.args(), ["sort", "/d/in.pbf", "-o", "/s/out.pbf", "-j", "4"]);
        assert_eq!(argv.reads(), [PathBuf::from("/d/in.pbf")]);
        assert_eq!(argv.writes(), [PathBuf::from("/s/out.pbf")]);
        assert_eq!(argv.overwrite(), Overwrite::Negotiate);
        assert!(!argv.writes_scratch());
    }

    #[test]
    fn an_extract_config_declares_its_outputs_and_reads_the_config() {
        let argv = PbfArgv::new("extract")
            .input("/d/in.pbf")
            .unwrap()
            .extract_config("/s/c.json", [PathBuf::from("/s/m/0.pbf"), PathBuf::from("/s/m/1.pbf")])
            .unwrap();
        assert_eq!(argv.args(), ["extract", "/d/in.pbf", "--config", "/s/c.json"]);
        assert_eq!(argv.reads().len(), 2);
        assert_eq!(argv.writes().len(), 2);
    }

    #[test]
    fn reads_and_forced_writes_decide_the_overwrite() {
        assert_eq!(PbfArgv::new("inspect").input("a").unwrap().overwrite(), Overwrite::NoOutput);
        let geocode = PbfArgv::new("build-geocode-index")
            .output_dir("--output-dir", "/s/g")
            .unwrap()
            .arg("--force")
            .forces_overwrite();
        assert_eq!(geocode.overwrite(), Overwrite::ImpliedByForce);
    }

    #[test]
    fn a_text_diff_writes_scratch_unless_quiet_or_one_job() {
        assert!(!text_diff_writes_scratch(Some(1), false));
        assert!(!text_diff_writes_scratch(None, true));
        assert!(text_diff_writes_scratch(None, false), "automatic jobs, whatever they resolve to");
        assert!(text_diff_writes_scratch(Some(0), false));
        assert!(text_diff_writes_scratch(Some(8), false));
    }
}
