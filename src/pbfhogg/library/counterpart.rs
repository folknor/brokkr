//! CLI equality (`docs/commands/library.md`, CLI equality): each call that
//! declares a CLI counterpart has it replayed with the same-commit pbfhogg
//! CLI after the harness exited, and every output it names digested on both
//! sides, giving `<call>/brokkr.cli`.
//!
//! Every launch here is run-scoped ([`PbfhoggBinary::run_in`]) and they are
//! strictly one at a time: each returns only after its release, and nothing
//! is launched while one is admitted. After every released launch the
//! directories it could have changed are checked without following a link,
//! whatever its exit, before anything below them is read again. A launch
//! whose admission or release did not establish a safe state stops the
//! whole run ([`Stop::Unsafe`]): from then on nothing in the run directory
//! is read or written.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::artefact::{self, Outcome, Status};
use super::describe::{Call, Cli, RoleSource, Scenario, Token};
use super::digest;
use super::run::{CWD_DIR, ROLES_DIR, printable};
use crate::measure::Supervised;
use crate::pbfhogg::argv::PbfArgv;
use crate::pbfhogg::binary::{PbfhoggBinary, RunScope};

/// The id brokkr gives its CLI equality result on each call with a
/// counterpart.
pub(crate) const CLI_RESULT: &str = "brokkr.cli";

/// The directory a call's replay gets in the run directory.
pub(crate) fn replay_dir_name(call: &str) -> String {
    format!("replay-{call}")
}

/// Where the replay's outputs go, inside its directory.
const OUTPUTS_DIR: &str = "outputs";

/// The stdout and stderr kept from a replay, for the report.
const REPLAY_OUTPUT_KEPT: usize = 64 * 1024;

/// The longest digest report read; a longer one is not a report.
const DIGEST_OUTPUT_KEPT: usize = 4096;

/// What stopped the run before every counterpart was judged.
#[derive(Debug)]
pub(crate) enum Stop {
    /// A launch whose admission or release did not establish a safe state,
    /// or a run directory that is no longer one: nothing more may be read or
    /// written in it.
    Unsafe(String),
    Interrupted,
}

/// What a measured run remembers about each counterpart between its
/// iterations (`docs/commands/library.md`, Measuring a scenario): the CLI
/// side's digest reports, under the reuse key they were made for.
#[derive(Debug, Default)]
pub(crate) struct Reuse {
    entries: BTreeMap<String, Reused>,
}

#[derive(Debug)]
struct Reused {
    key: String,
    iteration: usize,
    /// The CLI output's report, by output role.
    reports: BTreeMap<String, digest::Report>,
}

/// One measured iteration's view of the reuse cache: which iteration this
/// is, and the identity of every dataset and generated role (`absent` or a
/// digest), read after the harness exited and before any replay.
pub(crate) struct Reusing<'a> {
    pub(crate) reuse: &'a mut Reuse,
    pub(crate) iteration: usize,
    pub(crate) inputs: &'a BTreeMap<String, String>,
}

/// The reuse key of `counterpart` (R12): the CLI executable's XXH128, the
/// argv as declared (placeholders by role, never resolved paths), the
/// identity of every role an input or `files_in` placeholder names, and
/// the scratch flag. The initial output state is always absent, a fresh
/// brokkr path, so it is a constant of the key. `None` when an input has no
/// identity, which never reuses.
fn reuse_key(
    cli: &PbfhoggBinary,
    counterpart: &Cli,
    inputs: &BTreeMap<String, String>,
) -> Option<String> {
    let mut key = format!("cli {}\n", cli.digest());
    for token in &counterpart.argv {
        key.push_str(&format!("argv {}\n", token.show()));
        if let Token::Input(role) | Token::FilesIn(role) = token {
            key.push_str(&format!("input {role} {}\n", inputs.get(role)?));
        }
    }
    key.push_str(&format!("scratch {}\n", counterpart.scratch));
    Some(key)
}

/// Every counterpart's result, and the replay directories made.
#[derive(Debug, Default)]
pub(crate) struct Equality {
    pub(crate) results: BTreeMap<String, Outcome>,
    /// Names made in the run directory, for the final layout check.
    pub(crate) made: Vec<String>,
}

/// Replay each counterpart of `scenario` and compare. `bindings` maps every
/// role to its bound path; `layout_ok` says the run directory passed its
/// layout check after the harness (nothing is replayed otherwise, so no
/// role entry is opened unchecked). With `reusing`, a counterpart whose
/// reuse key matches an earlier iteration's is not replayed: its CLI
/// reports are that iteration's, and the library outputs are digested as
/// always.
pub(crate) fn judge(
    cli: &PbfhoggBinary,
    scenario: &Scenario,
    run_dir: &Path,
    bindings: &BTreeMap<String, PathBuf>,
    layout_ok: bool,
    mut reusing: Option<Reusing<'_>>,
) -> Result<Equality, Stop> {
    let mut equality = Equality::default();
    let mut breached: Option<String> = None;
    for call in &scenario.calls {
        let Some(counterpart) = &call.cli else {
            continue;
        };
        let mut verdict = Verdict::default();
        if !layout_ok {
            verdict.unevaluated("not replayed: the run directory failed its layout check".into());
        } else if let Some(why) = &breached {
            verdict.unevaluated(format!("not replayed: {why}"));
        } else {
            let replay = Replay {
                cli,
                scenario,
                call,
                counterpart,
                run_dir,
                bindings,
            };
            let key = reusing
                .as_ref()
                .and_then(|r| reuse_key(cli, counterpart, r.inputs));
            let cached = match (&reusing, &key) {
                (Some(r), Some(key)) => r
                    .reuse
                    .entries
                    .get(&call.id)
                    .filter(|e| e.key == *key)
                    .map(|e| (e.iteration, e.reports.clone())),
                _ => None,
            };
            let (breach, reports) = replay.run(&mut verdict, &mut equality.made, cached)?;
            if let Some(breach) = breach {
                breached = Some(format!(
                    "the replay of call {} left the run directory breached",
                    call.id
                ));
                verdict.fail(breach);
            }
            // Only a counterpart that passed is remembered, under the key its
            // replay was made for.
            if let (Some(r), Some(key), Some(reports)) = (reusing.as_mut(), key, reports)
                && !verdict.settled()
            {
                r.reuse.entries.insert(
                    call.id.clone(),
                    Reused {
                        key,
                        iteration: r.iteration,
                        reports,
                    },
                );
            }
        }
        equality
            .results
            .insert(call.id.clone(), verdict.outcome(counterpart));
    }
    Ok(equality)
}

/// A result being built: its status is the first defect's, and every
/// defect stays in its detail.
#[derive(Default)]
struct Verdict {
    status: Option<Status>,
    lines: Vec<String>,
}

impl Verdict {
    fn fail(&mut self, line: String) {
        self.status.get_or_insert(Status::Fail);
        self.lines.push(line);
    }

    fn unevaluated(&mut self, line: String) {
        self.status.get_or_insert(Status::Unevaluated);
        self.lines.push(line);
    }

    fn note(&mut self, line: String) {
        self.lines.push(line);
    }

    fn settled(&self) -> bool {
        self.status.is_some()
    }

    fn outcome(mut self, counterpart: &Cli) -> Outcome {
        if !counterpart.policy_differences.is_empty() {
            self.lines.push(format!(
                "declared policy differences: {}",
                counterpart.policy_differences.join("; ")
            ));
        }
        Outcome {
            id: CLI_RESULT.to_owned(),
            status: self.status.unwrap_or(Status::Pass),
            detail: self.lines.join("\n"),
        }
    }
}

struct Replay<'a> {
    cli: &'a PbfhoggBinary,
    scenario: &'a Scenario,
    call: &'a Call,
    counterpart: &'a Cli,
    run_dir: &'a Path,
    bindings: &'a BTreeMap<String, PathBuf>,
}

/// What one counterpart came to: a breach that ends every later replay,
/// and, when its replay ran and every output digested, the CLI reports by
/// output role.
type Ran = (Option<String>, Option<BTreeMap<String, digest::Report>>);

impl Replay<'_> {
    /// The replay and its digests, or with `cached` (the iteration whose CLI
    /// reports are reused, and those reports) the library digests alone.
    fn run(
        &self,
        verdict: &mut Verdict,
        made: &mut Vec<String>,
        cached: Option<(usize, BTreeMap<String, digest::Report>)>,
    ) -> Result<Ran, Stop> {
        let outputs: Vec<&str> = self
            .counterpart
            .argv
            .iter()
            .filter_map(|t| match t {
                Token::Output(role) => Some(role.as_str()),
                _ => None,
            })
            .collect();
        let name = replay_dir_name(&self.call.id);
        let replay = self.run_dir.join(&name);
        let cwd = replay.join(CWD_DIR);
        let out_dir = replay.join(OUTPUTS_DIR);

        // The library side first: an output it never made is a defect no
        // missing input can excuse.
        root(self.run_dir)?;
        for role in &outputs {
            match role_kind(self.run_dir, role) {
                Ok(Some(kind)) if kind.is_file() => {}
                Ok(Some(_)) => {
                    verdict.fail(format!("the library output {role} is not a regular file"));
                }
                Ok(None) => verdict.fail(format!("the library left no output {role}")),
                Err(e) => verdict.fail(e),
            }
        }
        if verdict.settled() {
            return Ok((None, None));
        }
        if let Some((iteration, reports)) = cached {
            return self.reused(&replay, made, name, iteration, &reports, &outputs, verdict);
        }
        let Some(argv) = self.argv(&out_dir, verdict) else {
            return Ok((None, None));
        };

        made.push(name);
        let created = [&replay, &cwd, &out_dir]
            .into_iter()
            .try_for_each(std::fs::create_dir);
        if let Err(e) = created {
            verdict.fail(format!("cannot lay out {}: {e}", replay.display()));
            return Ok((None, None));
        }
        let cwd_identity = identity(&cwd).map_err(|e| Stop::Unsafe(e.to_string()))?;
        let scope = RunScope {
            run_dir: self.run_dir,
            cwd: &cwd,
            deadline: std::time::Duration::from_secs(u64::from(self.scenario.deadline_s)),
            stdout_kept: REPLAY_OUTPUT_KEPT,
        };
        match self.cli.run_in(argv, &scope) {
            Supervised::Unsafe(e) => return Err(self.unsafe_launch("replay", &e)),
            Supervised::Interrupted => return Err(Stop::Interrupted),
            Supervised::Failed(e) => verdict.fail(format!("the counterpart did not run: {e}")),
            Supervised::Ran(capture) => {
                if let Some(why) = bad_ending(&capture.captured) {
                    verdict.fail(format!("the counterpart {why}"));
                }
            }
        }
        if let Some(breach) = self.validate(&replay, cwd_identity, &outputs, &outputs, true)? {
            return Ok((Some(breach), None));
        }
        for role in &outputs {
            match entry_kind(&out_dir.join(role)) {
                Ok(Some(kind)) if kind.is_file() => {}
                Ok(_) => verdict.fail(format!("the counterpart wrote no {role}")),
                Err(e) => verdict.fail(e),
            }
        }
        if verdict.settled() {
            return Ok((None, None));
        }

        let digest_scope = RunScope {
            stdout_kept: DIGEST_OUTPUT_KEPT,
            ..scope
        };
        self.compare(
            &digest_scope,
            &replay,
            cwd_identity,
            &outputs,
            None,
            verdict,
        )
    }

    /// A counterpart whose CLI reports iteration `iteration` already made
    /// under the same reuse key: `replay-<call>/` is still laid out, empty,
    /// as the library digests' working directory, and only the library
    /// outputs are digested, each against its reused report.
    #[allow(clippy::too_many_arguments)]
    fn reused(
        &self,
        replay: &Path,
        made: &mut Vec<String>,
        name: String,
        iteration: usize,
        reports: &BTreeMap<String, digest::Report>,
        outputs: &[&str],
        verdict: &mut Verdict,
    ) -> Result<Ran, Stop> {
        let cwd = replay.join(CWD_DIR);
        let out_dir = replay.join(OUTPUTS_DIR);
        made.push(name);
        let created = [replay, cwd.as_path(), out_dir.as_path()]
            .into_iter()
            .try_for_each(std::fs::create_dir);
        if let Err(e) = created {
            verdict.fail(format!("cannot lay out {}: {e}", replay.display()));
            return Ok((None, None));
        }
        let cwd_identity = identity(&cwd).map_err(|e| Stop::Unsafe(e.to_string()))?;
        verdict.note(format!(
            "CLI digests reused from iteration {iteration}: the same executable, argv, inputs \
             and scratch"
        ));
        let scope = RunScope {
            run_dir: self.run_dir,
            cwd: &cwd,
            deadline: std::time::Duration::from_secs(u64::from(self.scenario.deadline_s)),
            stdout_kept: DIGEST_OUTPUT_KEPT,
        };
        let breach = self
            .compare(
                &scope,
                replay,
                cwd_identity,
                outputs,
                Some(reports),
                verdict,
            )?
            .0;
        Ok((breach, None))
    }

    /// Digest each output on both sides, library first, and compare; with
    /// `cached`, the CLI side's reports are those and only the library side
    /// is digested. The CLI reports made, when every one was.
    fn compare(
        &self,
        digest_scope: &RunScope<'_>,
        replay: &Path,
        cwd_identity: (u64, u64),
        outputs: &[&str],
        cached: Option<&BTreeMap<String, digest::Report>>,
        verdict: &mut Verdict,
    ) -> Result<Ran, Stop> {
        let roles = self.run_dir.join(ROLES_DIR);
        let out_dir = replay.join(OUTPUTS_DIR);
        // A reused counterpart leaves outputs/ empty.
        let replay_outputs: &[&str] = if cached.is_some() { &[] } else { outputs };
        let mut made: BTreeMap<String, digest::Report> = BTreeMap::new();
        for role in outputs {
            let mut reports = Vec::new();
            let mut sides = vec![("library", roles.join(role))];
            if cached.is_none() {
                sides.push(("CLI", out_dir.join(role)));
            }
            for (what, file) in sides {
                let report = self.digest(digest_scope, &file)?;
                let breach = self.validate(replay, cwd_identity, replay_outputs, outputs, false)?;
                // The digest's own defect stays beside a breach it caused.
                match report {
                    Ok(report) => reports.push(report),
                    Err(e) => verdict.fail(format!("digest of the {what} output {role}: {e}")),
                }
                if breach.is_some() {
                    return Ok((breach, None));
                }
            }
            if let Some(cached) = cached {
                match cached.get(*role) {
                    Some(report) => reports.push(report.clone()),
                    None => verdict.fail(format!("no reused CLI report for {role}")),
                }
            }
            let [library, cli] = reports.as_slice() else {
                continue;
            };
            made.insert((*role).to_owned(), cli.clone());
            let differences = digest::differences(library, cli);
            if differences.is_empty() {
                let [(nodes, _), (ways, _), (relations, _)] = &library.kinds;
                verdict.note(format!(
                    "{role}: equal, {nodes} nodes, {ways} ways, {relations} relations"
                ));
            } else {
                verdict.fail(format!("{role} differs:"));
                for line in differences {
                    verdict.note(format!("  {line}"));
                }
            }
        }
        let complete = made.len() == outputs.len();
        Ok((None, complete.then_some(made)))
    }

    /// The counterpart's argv with every placeholder bound; `None` when an
    /// input it names cannot be read as declared (the result settled).
    fn argv(&self, out_dir: &Path, verdict: &mut Verdict) -> Option<PbfArgv> {
        let mut tokens = self.counterpart.argv.iter();
        let Some(Token::Literal(subcommand)) = tokens.next() else {
            verdict.fail("the counterpart has no subcommand".into());
            return None;
        };
        let mut argv = PbfArgv::new(subcommand);
        for token in tokens {
            let next = match token {
                Token::Literal(text) => Ok(argv.arg(text.clone())),
                Token::Input(role) => {
                    let Some(path) = self.bindings.get(role) else {
                        verdict.fail(format!("the input {role} was never bound"));
                        return None;
                    };
                    if self.generated(role) {
                        match role_kind(self.run_dir, role) {
                            Ok(Some(kind)) if kind.is_file() => {}
                            Ok(_) => {
                                verdict.unevaluated(format!(
                                    "the generated input {role} is absent or not a regular file"
                                ));
                                return None;
                            }
                            Err(e) => {
                                verdict.unevaluated(e);
                                return None;
                            }
                        }
                    }
                    argv.input(path)
                }
                Token::FilesIn(role) => {
                    let files = match self.files_in(role) {
                        Ok(files) => files,
                        Err(e) => {
                            verdict.unevaluated(e);
                            return None;
                        }
                    };
                    files.into_iter().try_fold(argv, PbfArgv::input)
                }
                Token::Output(role) => argv.written(out_dir.join(role)),
            };
            argv = match next {
                Ok(argv) => argv,
                Err(e) => {
                    verdict.fail(format!("cannot spell the counterpart's argv: {e}"));
                    return None;
                }
            };
        }
        if self.counterpart.scratch {
            argv = argv.scratch(out_dir);
        }
        Some(argv)
    }

    fn generated(&self, role: &str) -> bool {
        self.scenario
            .roles
            .iter()
            .any(|r| r.id == role && r.source == RoleSource::Generated)
    }

    /// Every entry of the generated directory `role`, each a regular file,
    /// sorted by the bytes of its name; at least one.
    fn files_in(&self, role: &str) -> Result<Vec<PathBuf>, String> {
        match role_kind(self.run_dir, role)? {
            Some(kind) if kind.is_dir() => {}
            _ => {
                return Err(format!(
                    "the generated directory {role} is absent or not a directory"
                ));
            }
        }
        let dir = self.run_dir.join(ROLES_DIR).join(role);
        let entries = artefact::entries(&dir)?;
        if entries.is_empty() {
            return Err(format!("the generated directory {role} is empty"));
        }
        let mut files = Vec::new();
        for (name, kind) in entries {
            if !kind.is_file() {
                return Err(format!("{role}/{} is not a regular file", printable(&name)));
            }
            files.push(dir.join(name));
        }
        Ok(files)
    }

    /// `pbfhogg digest FILE`, its report parsed; the inner error is this
    /// digest's defect.
    fn digest(
        &self,
        scope: &RunScope<'_>,
        file: &Path,
    ) -> Result<Result<digest::Report, String>, Stop> {
        let argv = match PbfArgv::new("digest").input(file) {
            Ok(argv) => argv,
            Err(e) => return Ok(Err(e.to_string())),
        };
        match self.cli.run_in(argv, scope) {
            Supervised::Unsafe(e) => Err(self.unsafe_launch("digest", &e)),
            Supervised::Interrupted => Err(Stop::Interrupted),
            Supervised::Failed(e) => Ok(Err(format!("did not run: {e}"))),
            Supervised::Ran(capture) => {
                if capture.output_cut {
                    return Ok(Err(
                        "left its output open after it exited, so its report is not known whole"
                            .to_owned(),
                    ));
                }
                if let Some(why) = bad_ending(&capture.captured) {
                    return Ok(Err(why));
                }
                if capture.captured.stdout.len() > DIGEST_OUTPUT_KEPT {
                    return Ok(Err(format!(
                        "printed more than {DIGEST_OUTPUT_KEPT} bytes, which no report is"
                    )));
                }
                Ok(digest::parse(&capture.captured.stdout))
            }
        }
    }

    fn unsafe_launch(&self, what: &str, e: &crate::error::DevError) -> Stop {
        Stop::Unsafe(format!(
            "the {what} launch of call {} did not establish a safe state: {e}; the run \
             directory is left untouched",
            self.call.id
        ))
    }

    /// After a released launch, whatever it came to: the run root is still a
    /// directory (else the run stops), the replay directory holds exactly
    /// `cwd/` (the same directory, empty) and `outputs/` (only the names in
    /// `outputs`, each a regular file, or absent while `missing_ok`), and
    /// every library output in `library` is still the regular file it was.
    /// A reused counterpart's `outputs` is empty, its `library` the declared
    /// roles. `Some` names the first breach.
    fn validate(
        &self,
        replay: &Path,
        cwd_identity: (u64, u64),
        outputs: &[&str],
        library: &[&str],
        missing_ok: bool,
    ) -> Result<Option<String>, Stop> {
        root(self.run_dir)?;
        let breach = |why: String| Ok(Some(format!("after a launch, {why}")));
        let shown = replay.display();
        match entry_kind(replay) {
            Ok(Some(kind)) if kind.is_dir() => {}
            Ok(_) => return breach(format!("{shown} is no longer a directory")),
            Err(e) => return breach(e),
        }
        let listed = match artefact::entries(replay) {
            Ok(listed) => listed,
            Err(e) => return breach(e),
        };
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        if names != [CWD_DIR, OUTPUTS_DIR] || !listed.iter().all(|(_, k)| k.is_dir()) {
            return breach(format!(
                "{shown} holds {names:?}, not exactly the directories cwd and outputs"
            ));
        }
        let cwd = replay.join(CWD_DIR);
        if identity(&cwd).ok() != Some(cwd_identity) {
            return breach(format!("{shown}/cwd is not the directory brokkr made"));
        }
        match artefact::entries(&cwd) {
            Ok(inside) if inside.is_empty() => {}
            Ok(_) => {
                return breach(format!(
                    "{shown}/cwd is not empty: the CLI left something in its working directory"
                ));
            }
            Err(e) => return breach(e),
        }
        let inside = match artefact::entries(&replay.join(OUTPUTS_DIR)) {
            Ok(inside) => inside,
            Err(e) => return breach(e),
        };
        for (name, kind) in &inside {
            if !outputs.contains(&name.as_str()) {
                return breach(format!(
                    "{shown}/outputs holds {:?}, which no output placeholder names",
                    printable(name)
                ));
            }
            if !kind.is_file() {
                return breach(format!("{shown}/outputs/{name} is not a regular file"));
            }
        }
        if !missing_ok && inside.len() != outputs.len() {
            return breach(format!("an output in {shown}/outputs went missing"));
        }
        for role in library {
            match role_kind(self.run_dir, role) {
                Ok(Some(kind)) if kind.is_file() => {}
                Ok(_) => {
                    return breach(format!(
                        "the library output {role} is no longer a regular file"
                    ));
                }
                Err(e) => return breach(e),
            }
        }
        Ok(None)
    }
}

/// The run root, never followed: a run directory that is no longer one
/// stops the run.
fn root(run_dir: &Path) -> Result<(), Stop> {
    match entry_kind(run_dir) {
        Ok(Some(kind)) if kind.is_dir() => Ok(()),
        _ => Err(Stop::Unsafe(format!(
            "{} is no longer the directory it was made as; nothing in it is read",
            crate::output::path(run_dir)
        ))),
    }
}

/// The kind of `roles/<role>`, `roles/` itself established as a directory
/// first, nothing followed. The caller has checked the root.
fn role_kind(run_dir: &Path, role: &str) -> Result<Option<std::fs::FileType>, String> {
    let roles = run_dir.join(ROLES_DIR);
    match entry_kind(&roles)? {
        Some(kind) if kind.is_dir() => entry_kind(&roles.join(role)),
        _ => Err(format!("{} is no longer a directory", roles.display())),
    }
}

fn entry_kind(path: &Path) -> Result<Option<std::fs::FileType>, String> {
    artefact::entry_kind(path)
}

fn identity(path: &Path) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::symlink_metadata(path)?;
    Ok((meta.dev(), meta.ino()))
}

/// What is wrong with a CLI child's ending, `None` for exit 0.
fn bad_ending(captured: &crate::output::CapturedOutput) -> Option<String> {
    use crate::output::child::Ending;
    // Raw: the report frames and escapes a result's detail line by line.
    let stderr = || {
        let text = String::from_utf8_lossy(&captured.stderr);
        let text = text.trim_end();
        if text.trim().is_empty() {
            String::new()
        } else {
            format!(", stderr:\n{text}")
        }
    };
    match &captured.ending {
        Ending::Exited(0) => None,
        Ending::Exited(code) => Some(format!("exited {code}{}", stderr())),
        Ending::Deadline(limit) => Some(format!(
            "was killed at its {} deadline",
            crate::output::format_duration(*limit)
        )),
        Ending::Signalled(signal) => Some(format!("was killed by signal {signal}{}", stderr())),
        other => Some(format!("ended as {other:?}")),
    }
}
