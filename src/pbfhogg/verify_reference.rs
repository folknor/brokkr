//! Verify reference cache: reuse a completed reference-tool execution.
//!
//! A verify check runs a reference tool (osmium, osmosis, osmconvert) and then
//! compares pbfhogg's output with the reference's through pbfhogg steps
//! (inspect, diff, the element diffs). The reference run is the expensive
//! half and changes only when its tool or inputs do, so its artifacts are kept
//! here and reused. Only the tool's execution is skipped: every pbfhogg step
//! still runs today, against the reused reference bytes. No pbfhogg
//! observation or verdict is cached.
//!
//! The contract is reuse of a completed execution of the same declared
//! reference computation, preserving its artifacts and its outcome - never a
//! claim that a rerun would write the same bytes.
//!
//! A run is declared as a [`ToolRun`]: literal arguments, inputs and outputs
//! kept apart, so the key can name each input by content and each output by
//! artifact name. The key folds the run id, the tool's identity (its
//! executable and the shared libraries `ldd` resolves for it, or for osmosis
//! its install tree, its Java runtime tree and that runtime's libraries), the
//! controlled environment, each input's content digest and file-name
//! extension, and the argv with those placeholders. A miss runs the tool in
//! a fresh staging directory with no inherited user environment, and
//! publishes the staging directory as the entry when the outcome is accepted
//! and the inputs and identity did not move during the run.
//!
//! A check never receives a cache path: each artifact is hard-linked into the
//! check's own directory (copied across filesystems), so the check owns its
//! names as it owns a fresh output, and removing or replacing an entry can
//! never take a file from a live check. Nothing writes a reference artifact
//! in place, so the shared inode is never mutated through a check's name.
//!
//! Integrity: the manifest records each artifact's xxh128 and a stamp of
//! device, inode, size and mtime. A lookup compares the stamp; a moved stamp
//! is rehashed against the recorded digest. ctime is not in the stamp, since
//! placing a hard link moves it. This detects a replaced or rewritten artifact
//! whose stamp moved; it does not detect an mtime-preserving in-place rewrite
//! or silent media corruption.
//!
//! Outside the identity, and so able to change a run without a miss: system
//! utilities a launcher script runs from the fixed `PATH`, libraries opened at
//! run time outside the `ldd` closure, the referents of symlinks inside a
//! hashed tree, and inputs or tools changed by something other than
//! cooperating brokkr processes while a run is in flight.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::DevError;
use crate::measure::{ExternalTool, Invocation};
use crate::output::{self, CapturedOutput};
use crate::tools::OsmosisTools;

use super::verify::VerifyHarness;

/// Bumped when the key text, the manifest or the launch environment changes
/// meaning, so entries written under another format are never read.
const FORMAT: u32 = 1;

const MANIFEST: &str = "manifest.json";
const ARTIFACTS: &str = "artifacts";
const STDOUT: &str = "stdout";
const STDERR: &str = "stderr";
const STAGING_PREFIX: &str = ".staging-";
const TRASH_PREFIX: &str = ".trash-";

/// The `PATH` a reference run sees. Tools are launched by absolute path; this
/// is for what a launcher script runs.
const RUN_PATH: &str = "/usr/bin:/bin";

/// Placeholder for the staging directory in the key's environment template.
const STAGING_PLACEHOLDER: &str = "<staging>";

static FRESH: AtomicBool = AtomicBool::new(false);

/// `brokkr verify --fresh-reference`: run every reference tool for real and
/// replace its entry.
pub(crate) fn set_fresh(fresh: bool) {
    FRESH.store(fresh, Ordering::Relaxed);
}

/// The reference program a [`ToolRun`] launches.
pub(crate) enum Program<'a> {
    Tool(ExternalTool),
    Osmosis(&'a OsmosisTools),
}

impl Program<'_> {
    fn name(&self) -> &'static str {
        match self {
            Self::Tool(tool) => tool.name(),
            Self::Osmosis(_) => "osmosis",
        }
    }
}

enum RunArg {
    Lit(String),
    Input { prefix: &'static str, path: PathBuf },
    Output { prefix: &'static str, path: PathBuf },
}

/// One declared reference run: its id (unique per check and run, and
/// versioned by the caller when the run's meaning changes), its program, its
/// argv with inputs and outputs marked, and the exit codes that complete it.
pub(crate) struct ToolRun<'a> {
    id: String,
    program: Program<'a>,
    args: Vec<RunArg>,
    accepted: Vec<i32>,
}

impl<'a> ToolRun<'a> {
    pub(crate) fn new(id: impl Into<String>, program: Program<'a>) -> Self {
        Self {
            id: id.into(),
            program,
            args: Vec::new(),
            accepted: vec![0],
        }
    }

    pub(crate) fn osmium(id: impl Into<String>) -> Self {
        Self::new(id, Program::Tool(ExternalTool::Osmium))
    }

    pub(crate) fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(RunArg::Lit(arg.into()));
        self
    }

    pub(crate) fn args(mut self, args: &[&str]) -> Self {
        self.args.extend(args.iter().map(|&a| RunArg::Lit(a.to_owned())));
        self
    }

    /// An input path, passed as its own argument.
    pub(crate) fn input(self, path: &Path) -> Self {
        self.input_as("", path)
    }

    /// An input path embedded after `prefix` in one argument (`file=`).
    pub(crate) fn input_as(mut self, prefix: &'static str, path: &Path) -> Self {
        self.args.push(RunArg::Input {
            prefix,
            path: path.to_path_buf(),
        });
        self
    }

    /// An output the run writes, at the path the check reads it from. Its
    /// file name is its artifact name.
    pub(crate) fn output(self, path: &Path) -> Self {
        self.output_as("", path)
    }

    /// An output embedded after `prefix` in one argument (`-o=`).
    pub(crate) fn output_as(mut self, prefix: &'static str, path: &Path) -> Self {
        self.args.push(RunArg::Output {
            prefix,
            path: path.to_path_buf(),
        });
        self
    }

    /// Exit codes beside 0 that complete the run (osmium diff and check-refs
    /// exit 1 for a found difference).
    pub(crate) fn accept(mut self, codes: &[i32]) -> Self {
        self.accepted.extend_from_slice(codes);
        self
    }

    /// The step label: the program and its first literal argument.
    fn label(&self) -> String {
        let first = self.args.iter().find_map(|a| match a {
            RunArg::Lit(s) if !s.starts_with('-') && !s.contains('=') => Some(s.as_str()),
            _ => None,
        });
        match (&self.program, first) {
            (Program::Tool(_), Some(first)) => format!("{} {first}", self.program.name()),
            _ => self.program.name().to_owned(),
        }
    }

    fn inputs(&self) -> Vec<&Path> {
        self.args
            .iter()
            .filter_map(|a| match a {
                RunArg::Input { path, .. } => Some(path.as_path()),
                _ => None,
            })
            .collect()
    }

    /// Each output's artifact name and the path the check reads it from.
    fn outputs(&self) -> Result<Vec<(String, &Path)>, DevError> {
        let mut out: Vec<(String, &Path)> = Vec::new();
        for arg in &self.args {
            let RunArg::Output { path, .. } = arg else {
                continue;
            };
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| ![MANIFEST, STDOUT, STDERR].contains(n) && !n.starts_with('.'))
                .ok_or_else(|| {
                    DevError::Internal(format!(
                        "reference output {} has no usable artifact name",
                        path.display()
                    ))
                })?
                .to_owned();
            if out.iter().any(|(n, _)| *n == name) {
                return Err(DevError::Internal(format!(
                    "reference run {} names artifact {name} twice",
                    self.id
                )));
            }
            out.push((name, path));
        }
        Ok(out)
    }
}

/// The variables every controlled launch sees, probes included.
fn base_env() -> Vec<(String, String)> {
    vec![
        ("PATH".to_owned(), RUN_PATH.to_owned()),
        ("LANG".to_owned(), "C.UTF-8".to_owned()),
        ("LC_ALL".to_owned(), "C.UTF-8".to_owned()),
        ("TZ".to_owned(), "UTC".to_owned()),
    ]
}

/// The run's environment beside brokkr's own lifecycle variables, with the
/// staging directory as `staging`. `java_props` passes osmosis its home and
/// temporary directory; it is off only where the launcher would split the
/// staging path, a run that is then never cached.
fn run_env(
    program: &Program<'_>,
    staging: &str,
    java_home: Option<&Path>,
    java_props: bool,
) -> Vec<(String, String)> {
    let mut env = base_env();
    env.push(("HOME".to_owned(), format!("{staging}/home")));
    env.push(("TMPDIR".to_owned(), format!("{staging}/scratch")));
    if let (Program::Osmosis(_), Some(java_home)) = (program, java_home) {
        env.push(("JAVA_HOME".to_owned(), java_home.display().to_string()));
        // The installed launcher reads JAVA_OPTS through xargs; the JVM does
        // not take user.home from HOME, and osmosis looks for plugins there.
        if java_props {
            env.push((
                "JAVA_OPTS".to_owned(),
                format!("-Duser.home={staging}/home -Djava.io.tmpdir={staging}/scratch"),
            ));
        }
    }
    env
}

/// Whether `path` survives the osmosis launcher's xargs word splitting intact.
fn xargs_safe(path: &str) -> bool {
    !path
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '\\'))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Manifest {
    format: u32,
    key: String,
    exit_code: i32,
    artifacts: Vec<Artifact>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Eq)]
struct Artifact {
    name: String,
    xxh128: String,
    stamp: Stamp,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i128,
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        }
    }
}

/// The cache, owned by the verify harness.
pub(crate) struct ReferenceCache {
    dir: PathBuf,
    project_root: PathBuf,
    /// Tool identities computed this invocation, by program; `None` when one
    /// could not be established, which runs that program uncached.
    identities: RefCell<BTreeMap<String, Option<String>>>,
    /// Programs already told they run uncached.
    warned: RefCell<std::collections::BTreeSet<String>>,
    seq: Cell<u32>,
}

impl ReferenceCache {
    pub(crate) fn new(project_root: &Path) -> Self {
        let dir = crate::clean::layout::ProjectState::new(project_root).verify_reference_dir();
        sweep_leftovers(&dir);
        Self {
            dir,
            project_root: project_root.to_path_buf(),
            identities: RefCell::new(BTreeMap::new()),
            warned: RefCell::new(std::collections::BTreeSet::new()),
            seq: Cell::new(0),
        }
    }

    /// Run `run` for `harness`, its stdout at `stdout`, reusing a cached
    /// execution when one matches. Returns the run's outcome as a fresh run
    /// would; the caller judges it as before.
    pub(crate) fn run(
        &self,
        harness: &VerifyHarness,
        run: &ToolRun<'_>,
        stdout: &Path,
    ) -> Result<CapturedOutput, DevError> {
        let label = run.label();
        let outputs = run.outputs()?;
        let staging = self.next_staging();
        let java_props = xargs_safe(&staging.display().to_string());
        let key = if matches!(run.program, Program::Osmosis(_)) && !java_props {
            self.say_uncached(run.program.name(), "the cache path holds characters its launcher splits");
            None
        } else {
            self.key(run)?
        };
        let Some(key) = key else {
            return self.miss(harness, run, &label, &outputs, stdout, &staging, None, java_props);
        };
        let entry = self.dir.join(&key.hex);
        if !FRESH.load(Ordering::Relaxed)
            && let Some(manifest) = self.lookup(&entry, &key, run, &outputs)
        {
            let _step = super::verify::cached_step(&label);
            place(&entry.join(ARTIFACTS), &outputs, stdout, true)?;
            touch(&entry.join(MANIFEST));
            output::verify_msg(&format!(
                "  {label}: cached reference {} (exit {})",
                &key.hex[..12],
                manifest.exit_code
            ));
            let stderr = fs::read(entry.join(ARTIFACTS).join(STDERR))?;
            return Ok(CapturedOutput {
                ending: output::child::Ending::Exited(manifest.exit_code),
                stdout: Vec::new(),
                stderr,
                elapsed: std::time::Duration::ZERO,
            });
        }
        self.miss(harness, run, &label, &outputs, stdout, &staging, Some(&key), java_props)
    }

    /// Run the tool for real in `staging`, place its artifacts, and publish
    /// the run under `key` when there is one and the run may be cached. A run
    /// without a key launches the same way and is never published. Only an
    /// interrupt, the tool's own failure to run, or a placement failure is an
    /// error; a cache-storage failure is a warning.
    #[allow(clippy::too_many_arguments)]
    fn miss(
        &self,
        harness: &VerifyHarness,
        run: &ToolRun<'_>,
        label: &str,
        outputs: &[(String, &Path)],
        stdout: &Path,
        staging: &Path,
        key: Option<&Key>,
        java_props: bool,
    ) -> Result<CapturedOutput, DevError> {
        let artifacts = staging.join(ARTIFACTS);
        let launched = ["", ARTIFACTS, "home", "scratch"]
            .iter()
            .try_for_each(|sub| fs::create_dir_all(staging.join(sub)))
            .map_err(DevError::from)
            .and_then(|()| self.launch(harness, run, label, staging, java_props));
        let captured = match launched {
            Ok(captured) => captured,
            Err(e) => {
                remove_tree(staging);
                return Err(e);
            }
        };
        let completed = captured
            .code()
            .is_some_and(|code| run.accepted.contains(&code))
            && outputs.iter().all(|(name, _)| artifacts.join(name).is_file());
        let mut cacheable = key.is_some() && completed;
        if cacheable && let Err(e) = fs::write(artifacts.join(STDERR), &captured.stderr) {
            output::warn(&format!("{label}: cannot keep its stderr, so it is not cached: {e}"));
            cacheable = false;
        }
        if let (true, Some(key)) = (cacheable, key) {
            match self.key_uncached(run) {
                Ok(Some(now)) if now.hex == key.hex => {}
                Err(DevError::Interrupted) => {
                    remove_tree(staging);
                    return Err(DevError::Interrupted);
                }
                _ => {
                    output::warn(&format!(
                        "{label}: an input or the tool changed while it ran; its result is \
                         used but not cached"
                    ));
                    cacheable = false;
                }
            }
        }
        let mut source = artifacts.clone();
        if let (true, Some(key)) = (cacheable, key) {
            let entry = self.dir.join(&key.hex);
            if let Err(e) = publish(staging, &entry, &key.text, captured.code().unwrap_or(0), outputs) {
                output::warn(&format!("{label}: cannot cache the reference run: {e}"));
            }
            // Wherever the artifacts survived: a failure after the rename
            // leaves them in the entry, not in staging.
            if !staging.exists() {
                source = entry.join(ARTIFACTS);
            }
        }
        let placed = place(&source, outputs, stdout, completed);
        if staging.exists() {
            remove_tree(staging);
        }
        placed?;
        Ok(captured)
    }

    /// The tool run in staging, with the controlled environment, reserving
    /// its inputs for reading and staging for writing.
    fn launch(
        &self,
        harness: &VerifyHarness,
        run: &ToolRun<'_>,
        label: &str,
        staging: &Path,
        java_props: bool,
    ) -> Result<CapturedOutput, DevError> {
        let staging_str = staging.display().to_string();
        let mut args = Vec::with_capacity(run.args.len());
        let mut reads = Vec::new();
        for arg in &run.args {
            args.push(match arg {
                RunArg::Lit(s) => s.clone(),
                RunArg::Input { prefix, path } => {
                    let resolved = absolute(path)?;
                    let text = format!("{prefix}{}", resolved.display());
                    reads.push(resolved);
                    text
                }
                RunArg::Output { prefix, path } => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    format!("{prefix}{staging_str}/{ARTIFACTS}/{name}")
                }
            });
        }
        let (program, java_home) = match &run.program {
            Program::Tool(tool) => (resolve_tool(*tool)?, None),
            Program::Osmosis(tools) => (
                fs::canonicalize(&tools.osmosis)?,
                Some(fs::canonicalize(&tools.java_home)?),
            ),
        };
        let spec = crate::reservations::LaunchSpec::new(staging.to_path_buf(), reads, Vec::new(), None)
            .with_coverage(vec![staging.to_path_buf()]);
        let mut invocation = Invocation::reserved(&program, args, spec).isolated_env();
        for (k, v) in run_env(&run.program, &staging_str, java_home.as_deref(), java_props) {
            invocation = invocation.with_env(&k, &v);
        }
        harness.run_reference_in(&invocation, label, &staging.join(ARTIFACTS).join(STDOUT), staging)
    }

    /// Say once per program that its runs are not cached, and why.
    fn say_uncached(&self, name: &str, why: &str) {
        if self.warned.borrow_mut().insert(name.to_owned()) {
            output::warn(&format!("{name}: {why}, so its reference runs are not cached"));
        }
    }

    fn next_staging(&self) -> PathBuf {
        let n = self.seq.get();
        self.seq.set(n + 1);
        self.dir
            .join(format!("{STAGING_PREFIX}{}-{n}", std::process::id()))
    }

    /// The run's key, or `None` when it runs uncached (said once per program).
    fn key(&self, run: &ToolRun<'_>) -> Result<Option<Key>, DevError> {
        let name = run.program.name().to_owned();
        let known = self.identities.borrow().get(&name).cloned();
        let identity = match known {
            Some(identity) => identity,
            None => {
                let identity = match identity(&run.program, &self.project_root) {
                    Ok(identity) => identity,
                    Err(DevError::Interrupted) => return Err(DevError::Interrupted),
                    Err(e) => {
                        output::verify_msg(&format!("  {name}: identity unavailable: {e}"));
                        None
                    }
                };
                self.identities.borrow_mut().insert(name.clone(), identity.clone());
                identity
            }
        };
        let Some(identity) = identity else {
            self.say_uncached(&name, "its identity cannot be established");
            return Ok(None);
        };
        key_text(run, &identity, &self.project_root).map(Some)
    }

    /// [`key`](Self::key) with the identity computed again, for the check
    /// after a run that nothing it depends on moved.
    fn key_uncached(&self, run: &ToolRun<'_>) -> Result<Option<Key>, DevError> {
        match identity(&run.program, &self.project_root)? {
            Some(identity) => key_text(run, &identity, &self.project_root).map(Some),
            None => Ok(None),
        }
    }

    /// The entry at `entry` when it is complete, valid for `key` and today's
    /// accepted outcomes; an invalid one is removed.
    fn lookup(
        &self,
        entry: &Path,
        key: &Key,
        run: &ToolRun<'_>,
        outputs: &[(String, &Path)],
    ) -> Option<Manifest> {
        if let Err(e) = fs::symlink_metadata(entry)
            && e.kind() == std::io::ErrorKind::NotFound
        {
            return None;
        }
        let verdict = (|| -> Result<Manifest, String> {
            let text = fs::read_to_string(entry.join(MANIFEST))
                .map_err(|e| format!("manifest unreadable: {e}"))?;
            let mut manifest: Manifest =
                serde_json::from_str(&text).map_err(|e| format!("manifest unreadable: {e}"))?;
            if manifest.format != FORMAT || manifest.key != key.text {
                return Err("manifest does not match the key".into());
            }
            if !run.accepted.contains(&manifest.exit_code) {
                return Err(format!("stored exit {} is not accepted", manifest.exit_code));
            }
            let mut expected: Vec<&str> = outputs.iter().map(|(n, _)| n.as_str()).collect();
            expected.extend([STDOUT, STDERR]);
            expected.sort_unstable();
            let mut held: Vec<&str> = manifest.artifacts.iter().map(|a| a.name.as_str()).collect();
            held.sort_unstable();
            if held != expected {
                return Err("artifact set differs from the run's".into());
            }
            let mut restamped = false;
            for artifact in &mut manifest.artifacts {
                let path = entry.join(ARTIFACTS).join(&artifact.name);
                let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", artifact.name))?;
                if !meta.is_file() {
                    return Err(format!("{} is not a regular file", artifact.name));
                }
                let stamp = Stamp::of(&meta);
                if stamp == artifact.stamp {
                    continue;
                }
                let digest = crate::preflight::digest::compute_file_digest(&path)
                    .map_err(|e| format!("{}: {e}", artifact.name))?;
                if digest.to_string() != artifact.xxh128 {
                    return Err(format!("{} no longer matches its digest", artifact.name));
                }
                artifact.stamp = stamp;
                restamped = true;
            }
            if restamped {
                write_manifest(&entry.join(MANIFEST), &manifest).map_err(|e| e.to_string())?;
            }
            Ok(manifest)
        })();
        match verdict {
            Ok(manifest) => Some(manifest),
            Err(why) => {
                output::verify_msg(&format!("  reference cache entry dropped: {why}"));
                remove_tree(entry);
                None
            }
        }
    }
}

struct Key {
    text: String,
    hex: String,
}

/// The key text and its hash. Inputs are named by content and file-name
/// extension (format detection keys on the suffix), outputs by artifact name.
fn key_text(run: &ToolRun<'_>, identity: &str, project_root: &Path) -> Result<Key, DevError> {
    let mut text = format!("brokkr-verify-reference {FORMAT}\nrun {}\n{identity}", run.id);
    for (k, v) in run_env(&run.program, STAGING_PLACEHOLDER, Some(Path::new("<java_home>")), true) {
        text.push_str(&format!("env {k}={v}\n"));
    }
    // The resolved path, as the launch passes it: its suffix is what the
    // tool's format detection sees.
    for (i, input) in run.inputs().iter().enumerate() {
        let resolved = absolute(input)?;
        let digest = crate::preflight::digest::cached_path_digest(&resolved, project_root)?;
        text.push_str(&format!("in {i} {digest} {}\n", extension(&resolved)));
    }
    let mut input_index = 0;
    for arg in &run.args {
        let rendered = match arg {
            RunArg::Lit(s) => s.clone(),
            RunArg::Input { prefix, .. } => {
                input_index += 1;
                format!("{prefix}<in{}>", input_index - 1)
            }
            RunArg::Output { prefix, path } => format!(
                "{prefix}<out:{}>",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
        };
        text.push_str(&format!("arg {rendered}\n"));
    }
    let hash = xxhash_rust::xxh3::xxh3_128(text.as_bytes());
    Ok(Key {
        text,
        hex: format!("{hash:032x}"),
    })
}

/// Everything from a file name's first dot: `.osm.pbf`, `.osc.gz`.
fn extension(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    match name.find('.') {
        Some(at) => name[at..].to_owned(),
        None => String::new(),
    }
}

fn absolute(path: &Path) -> Result<PathBuf, DevError> {
    fs::canonicalize(path)
        .map_err(|e| DevError::Verify(format!("cannot resolve reference input {}: {e}", path.display())))
}

fn resolve_tool(tool: ExternalTool) -> Result<PathBuf, DevError> {
    let found = crate::tools::find_executable(tool.name())
        .ok_or_else(|| DevError::Verify(format!("{} is not on PATH", tool.name())))?;
    Ok(fs::canonicalize(found)?)
}

/// The identity text of `program`, or `None` when it cannot be established.
fn identity(program: &Program<'_>, project_root: &Path) -> Result<Option<String>, DevError> {
    let digest = |path: &Path| crate::preflight::digest::cached_path_digest(path, project_root);
    let mut text = String::new();
    match program {
        Program::Tool(tool) => {
            let Some(found) = crate::tools::find_executable(tool.name()) else {
                return Ok(None);
            };
            let path = fs::canonicalize(found)?;
            text.push_str(&format!("tool {} {} {}\n", tool.name(), path.display(), digest(&path)?));
            if matches!(tool, ExternalTool::Osmium) {
                let Some(version) = probe(&path, &["--version"], project_root)? else {
                    return Ok(None);
                };
                text.push_str(&format!("version {}\n", version.trim()));
            }
            let Some(libs) = ldd_closure(&path, project_root)? else {
                return Ok(None);
            };
            for lib in libs {
                text.push_str(&format!("lib {} {}\n", lib.display(), digest(&lib)?));
            }
        }
        Program::Osmosis(tools) => {
            let launcher = fs::canonicalize(&tools.osmosis)?;
            let Some(root) = launcher.parent().and_then(Path::parent) else {
                return Ok(None);
            };
            let java_home = fs::canonicalize(&tools.java_home)?;
            text.push_str(&format!(
                "osmosis {} {}\n",
                root.display(),
                crate::preflight::digest::compute_tree_digest(root, project_root)?
            ));
            text.push_str(&format!(
                "java {} {}\n",
                java_home.display(),
                crate::preflight::digest::compute_tree_digest(&java_home, project_root)?
            ));
            for binary in [java_home.join("bin/java"), java_home.join("lib/server/libjvm.so")] {
                if !binary.is_file() {
                    continue;
                }
                let Some(libs) = ldd_closure(&binary, project_root)? else {
                    return Ok(None);
                };
                for lib in libs {
                    text.push_str(&format!("lib {} {}\n", lib.display(), digest(&lib)?));
                }
            }
        }
    }
    Ok(Some(text))
}

/// The shared libraries `ldd` resolves for `binary`, canonicalized and
/// sorted; `None` when ldd fails or leaves one unresolved.
fn ldd_closure(binary: &Path, project_root: &Path) -> Result<Option<Vec<PathBuf>>, DevError> {
    let Some(ldd) = crate::tools::find_executable("ldd") else {
        return Ok(None);
    };
    let Some(text) = probe(&ldd, &[&binary.display().to_string()], project_root)? else {
        return Ok(None);
    };
    match parse_ldd(&text) {
        Some(paths) => {
            let mut libs = Vec::new();
            for path in paths {
                libs.push(fs::canonicalize(&path)?);
            }
            libs.sort();
            libs.dedup();
            Ok(Some(libs))
        }
        None => Ok(None),
    }
}

/// The library paths in `ldd` output; `None` when a library is not found.
fn parse_ldd(text: &str) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains("not found") {
            return None;
        }
        let target = match line.split_once("=>") {
            Some((_, rest)) => rest.split_whitespace().next(),
            None => line.split_whitespace().next(),
        };
        if let Some(target) = target.filter(|t| t.starts_with('/')) {
            paths.push(PathBuf::from(target));
        }
    }
    Some(paths)
}

/// A short identity probe's stdout, run under the same controlled
/// environment as a reference run, so it sees the loader and the libraries
/// the run will; `None` when it exits non-zero.
fn probe(program: &Path, args: &[&str], project_root: &Path) -> Result<Option<String>, DevError> {
    let dir = crate::clean::layout::ProjectState::new(project_root).verify_reference_dir();
    fs::create_dir_all(&dir)?;
    let sink = dir.join(format!("{STAGING_PREFIX}probe-{}", std::process::id()));
    // Reserved like a reference run, so a descendant still writing the sink
    // holds the lease and the run fails rather than handing back part of it.
    let spec = crate::reservations::LaunchSpec::new(dir.clone(), Vec::new(), Vec::new(), None)
        .with_coverage(vec![sink.clone()]);
    let mut invocation =
        Invocation::reserved(program, args.iter().map(|&a| a.to_owned()).collect(), spec)
            .isolated_env();
    for (k, v) in base_env() {
        invocation = invocation.with_env(&k, &v);
    }
    let outcome = (|| -> Result<Option<String>, DevError> {
        let run = invocation.run_to_file(&dir, &sink)?;
        if let Some(failure) = run.capture_failure(&program.display().to_string()) {
            return Err(failure);
        }
        if run.captured.code() != Some(0) {
            return Ok(None);
        }
        Ok(Some(fs::read_to_string(&sink)?))
    })();
    drop(crate::reservations::remove_file(&sink));
    outcome
}

/// Seal `staging` and make it the entry at `entry`, replacing any older one.
fn publish(
    staging: &Path,
    entry: &Path,
    key: &str,
    exit_code: i32,
    outputs: &[(String, &Path)],
) -> Result<(), DevError> {
    let artifacts_dir = staging.join(ARTIFACTS);
    let mut names: Vec<&str> = outputs.iter().map(|(n, _)| n.as_str()).collect();
    names.extend([STDOUT, STDERR]);
    let mut artifacts = Vec::new();
    for name in names {
        let path = artifacts_dir.join(name);
        let meta = fs::symlink_metadata(&path)?;
        if !meta.is_file() {
            return Err(DevError::Verify(format!("{name} is not a regular file")));
        }
        fs::File::open(&path)?.sync_all()?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
        let xxh128 = crate::preflight::digest::compute_file_digest(&path)?.to_string();
        artifacts.push(Artifact {
            name: name.to_owned(),
            xxh128,
            stamp: Stamp::of(&fs::symlink_metadata(&path)?),
        });
    }
    // Only the artifacts and the manifest are published.
    for scratch in ["home", "scratch"] {
        crate::reservations::remove_dir_all(&staging.join(scratch))?;
    }
    write_manifest(
        &staging.join(MANIFEST),
        &Manifest {
            format: FORMAT,
            key: key.to_owned(),
            exit_code,
            artifacts,
        },
    )?;
    sync_dir(&artifacts_dir)?;
    sync_dir(staging)?;
    if entry.exists() {
        let trash = entry.with_file_name(format!(
            "{TRASH_PREFIX}{}",
            entry.file_name().unwrap_or_default().to_string_lossy()
        ));
        crate::reservations::rename(entry, &trash)?;
        crate::reservations::rename(staging, entry)?;
        remove_tree(&trash);
    } else {
        crate::reservations::rename(staging, entry)?;
    }
    if let Some(parent) = entry.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<(), DevError> {
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(serde_json::to_string_pretty(manifest).map_err(|e| DevError::Internal(e.to_string()))?.as_bytes())?;
    file.sync_all()?;
    crate::reservations::rename(&tmp, path)?;
    Ok(())
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Put each artifact of `source` at its check path, and stdout at `stdout`.
/// Destinations must be absent. `complete` requires every artifact; otherwise
/// (a run that was not published) only the ones the run wrote are placed.
/// On failure the names this placement made are removed.
fn place(
    source: &Path,
    outputs: &[(String, &Path)],
    stdout: &Path,
    complete: bool,
) -> Result<(), DevError> {
    let mut targets: Vec<(&str, &Path)> = outputs.iter().map(|(n, p)| (n.as_str(), *p)).collect();
    targets.push((STDOUT, stdout));
    let mut made: Vec<PathBuf> = Vec::new();
    let result = (|| -> Result<(), DevError> {
        for (name, dest) in targets {
            let src = source.join(name);
            if !src.exists() {
                if complete {
                    return Err(DevError::Internal(format!("cached artifact {name} is missing")));
                }
                continue;
            }
            place_one(&src, dest)?;
            made.push(dest.to_path_buf());
        }
        Ok(())
    })();
    if result.is_err() {
        for path in made {
            drop(crate::reservations::remove_file(&path));
        }
    }
    result
}

fn place_one(src: &Path, dest: &Path) -> Result<(), DevError> {
    if fs::symlink_metadata(dest).is_ok() {
        return Err(DevError::Internal(format!(
            "reference artifact destination {} already exists",
            dest.display()
        )));
    }
    match fs::hard_link(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let tmp = dest.with_file_name(format!(
                ".{}.reference-copy",
                dest.file_name().unwrap_or_default().to_string_lossy()
            ));
            let copied = (|| -> Result<(), DevError> {
                fs::copy(src, &tmp)?;
                fs::File::open(&tmp)?.sync_all()?;
                if fs::symlink_metadata(dest).is_ok() {
                    return Err(DevError::Internal(format!(
                        "reference artifact destination {} appeared during the copy",
                        dest.display()
                    )));
                }
                crate::reservations::rename(&tmp, dest)?;
                Ok(())
            })();
            if copied.is_err() {
                drop(crate::reservations::remove_file(&tmp));
            }
            copied
        }
        Err(e) => Err(DevError::Verify(format!(
            "cannot place reference artifact {}: {e}",
            dest.display()
        ))),
    }
}

/// Mark an entry used, for a future recency policy.
fn touch(manifest: &Path) {
    if let Ok(file) = fs::File::options().write(true).open(manifest) {
        drop(file.set_modified(std::time::SystemTime::now()));
    }
}

fn remove_tree(path: &Path) {
    if let Err(e) = crate::reservations::remove_dir_all(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        output::warn(&format!("cannot remove {}: {e}", path.display()));
    }
}

/// Remove staging and trash a killed verify left. Its reference tools died
/// with it (they are spawned to die with their parent), and verify runs under
/// the brokkr lock, so nothing is writing into them.
fn sweep_leftovers(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(STAGING_PREFIX) || name.starts_with(TRASH_PREFIX) {
            remove_tree(&entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn ldd_output_parses_resolved_libraries_and_refuses_a_missing_one() {
        let text = "\tlinux-vdso.so.1 (0x00007ffd)\n\
                    \tlibz.so.1 => /lib/x86_64-linux-gnu/libz.so.1 (0x00007f)\n\
                    \t/lib64/ld-linux-x86-64.so.2 (0x00007f)\n";
        assert_eq!(
            parse_ldd(text).unwrap(),
            vec![
                PathBuf::from("/lib/x86_64-linux-gnu/libz.so.1"),
                PathBuf::from("/lib64/ld-linux-x86-64.so.2")
            ]
        );
        assert!(parse_ldd("\tlibfoo.so => not found\n").is_none());
    }

    #[test]
    fn extensions_keep_every_suffix() {
        assert_eq!(extension(Path::new("/d/denmark.osm.pbf")), ".osm.pbf");
        assert_eq!(extension(Path::new("x.osc.gz")), ".osc.gz");
        assert_eq!(extension(Path::new("plain")), "");
    }

    #[test]
    fn xargs_safety_refuses_splitting_characters() {
        assert!(xargs_safe("/home/folk/p/state/verify-reference/.staging-1-0"));
        assert!(!xargs_safe("/home/my project/x"));
        assert!(!xargs_safe("/a'b"));
        assert!(!xargs_safe("/a\\b"));
    }

    #[test]
    fn the_key_names_inputs_by_content_and_outputs_by_artifact() {
        let root = crate::test_scratch::scratch("verify-reference", "key");
        let input = root.join("in.osm.pbf");
        fs::write(&input, b"one").unwrap();
        let run = |input: &Path, out: &str| {
            ToolRun::osmium("sort.1")
                .arg("sort")
                .input(input)
                .arg("-o")
                .output(&root.join("a").join(out))
        };
        let a = key_text(&run(&input, "osmium.osm.pbf"), "tool x\n", &root).unwrap();
        // Another check directory, same artifact name: the same key.
        let moved = ToolRun::osmium("sort.1")
            .arg("sort")
            .input(&input)
            .arg("-o")
            .output(&root.join("b").join("osmium.osm.pbf"));
        assert_eq!(a.hex, key_text(&moved, "tool x\n", &root).unwrap().hex);
        // Another tool identity, another artifact name or other input bytes:
        // another key.
        assert_ne!(a.hex, key_text(&run(&input, "osmium.osm.pbf"), "tool y\n", &root).unwrap().hex);
        assert_ne!(a.hex, key_text(&run(&input, "other.osm.pbf"), "tool x\n", &root).unwrap().hex);
        let other = root.join("other.osm.pbf");
        fs::write(&other, b"two").unwrap();
        assert_ne!(a.hex, key_text(&run(&other, "osmium.osm.pbf"), "tool x\n", &root).unwrap().hex);
        assert!(a.text.contains("arg <out:osmium.osm.pbf>"), "{}", a.text);
        assert!(a.text.contains("arg <in0>"), "{}", a.text);
    }

    #[test]
    fn placement_links_into_an_absent_name_and_refuses_an_existing_one() {
        let root = crate::test_scratch::scratch("verify-reference", "place");
        let source = root.join("artifacts");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("osmium.osm.pbf"), b"pbf").unwrap();
        fs::write(source.join(STDOUT), b"out").unwrap();
        let check = root.join("check");
        fs::create_dir_all(&check).unwrap();
        let out = check.join("osmium.osm.pbf");
        let outputs = vec![("osmium.osm.pbf".to_owned(), out.as_path())];
        place(&source, &outputs, &check.join("osmium.stdout"), true).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"pbf");
        assert_eq!(
            fs::metadata(&out).unwrap().ino(),
            fs::metadata(source.join("osmium.osm.pbf")).unwrap().ino()
        );
        // A second placement over the same names is refused and leaves the
        // first in place.
        assert!(place(&source, &outputs, &check.join("osmium.stdout"), true).is_err());
        assert!(out.exists());
    }

    #[test]
    fn a_published_entry_is_found_and_a_tampered_one_is_dropped() {
        let root = crate::test_scratch::scratch("verify-reference", "publish");
        let staging = root.join(".staging-1-0");
        for sub in [ARTIFACTS, "home", "scratch"] {
            fs::create_dir_all(staging.join(sub)).unwrap();
        }
        for (name, body) in [("osmium.osm.pbf", "pbf"), (STDOUT, ""), (STDERR, "")] {
            fs::write(staging.join(ARTIFACTS).join(name), body).unwrap();
        }
        let check = root.join("check");
        fs::create_dir_all(&check).unwrap();
        let out = check.join("osmium.osm.pbf");
        let outputs = vec![("osmium.osm.pbf".to_owned(), out.as_path())];
        let run = ToolRun::osmium("t.1").arg("sort").output(&out);
        let key = Key {
            text: "k".into(),
            hex: "00".into(),
        };
        let entry = root.join("00");
        publish(&staging, &entry, &key.text, 0, &outputs).unwrap();
        assert!(!staging.exists());
        assert!(!entry.join("home").exists());
        let cache = ReferenceCache {
            dir: root.clone(),
            project_root: root.clone(),
            identities: RefCell::new(BTreeMap::new()),
            warned: RefCell::new(std::collections::BTreeSet::new()),
            seq: Cell::new(0),
        };
        assert!(cache.lookup(&entry, &key, &run, &outputs).is_some());
        // Linking moves ctime only; the entry stays valid.
        place(&entry.join(ARTIFACTS), &outputs, &check.join("o.stdout"), true).unwrap();
        assert!(cache.lookup(&entry, &key, &run, &outputs).is_some());
        // Replacing an artifact's bytes moves its stamp; the rehash fails it.
        let artifact = entry.join(ARTIFACTS).join("osmium.osm.pbf");
        crate::reservations::remove_file(&artifact).unwrap();
        fs::write(&artifact, b"different").unwrap();
        assert!(cache.lookup(&entry, &key, &run, &outputs).is_none());
        assert!(!entry.exists());
        // The check's own link kept the original bytes.
        assert_eq!(fs::read(&out).unwrap(), b"pbf");
    }

    #[test]
    fn a_stored_exit_outside_todays_accepted_set_is_a_miss() {
        let root = crate::test_scratch::scratch("verify-reference", "accepted");
        let staging = root.join(".staging-1-0");
        for sub in [ARTIFACTS, "home", "scratch"] {
            fs::create_dir_all(staging.join(sub)).unwrap();
        }
        for name in [STDOUT, STDERR] {
            fs::write(staging.join(ARTIFACTS).join(name), "").unwrap();
        }
        let entry = root.join("01");
        publish(&staging, &entry, "k", 1, &[]).unwrap();
        let cache = ReferenceCache {
            dir: root.clone(),
            project_root: root.clone(),
            identities: RefCell::new(BTreeMap::new()),
            warned: RefCell::new(std::collections::BTreeSet::new()),
            seq: Cell::new(0),
        };
        let key = Key {
            text: "k".into(),
            hex: "01".into(),
        };
        let strict = ToolRun::osmium("t.1").arg("diff");
        assert!(cache.lookup(&entry, &key, &strict, &[]).is_none());
    }
}
