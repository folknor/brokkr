//! `[[install]]` steps: what `brokkr install` does after every package's
//! `cargo install` succeeded. User-facing contract: `docs/commands/run.md`.
//!
//! A `run` step is a shell command. A `copy` step publishes a file or
//! directory from the project as an artifact brokkr owns exclusively: it
//! replaces the destination wholesale, so a file removed from the source is
//! gone from the install, and it does so only when the destination is absent
//! or holds exactly what this step last installed there. Ownership is content,
//! not identity - the registry ([`registry`]) records the fingerprint
//! ([`tree`]) of what was published, so a destination someone edited, or one
//! brokkr never installed, is refused rather than overwritten. Publication and
//! crash recovery live in [`transaction`].
//!
//! Every step is checked before the first `cargo install` (preflight), and
//! each copy step is checked again just before it runs, since an earlier step
//! may have changed the filesystem. A copy source an earlier `run` step may
//! generate is allowed to be missing at preflight.

mod registry;
mod transaction;
mod tree;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::{InstallAction, InstallStep};
use crate::error::DevError;
use crate::output;
use registry::{Owner, Registry};
use tree::Observed;

/// The project side of an install: its steps and where they resolve.
pub struct Steps<'a> {
    pub steps: &'a [InstallStep],
    pub project: &'a str,
    /// Where `brokkr.toml` lives; `copy.from` resolves against it.
    pub project_root: &'a Path,
}

/// The steps, checked and ready to run after the packages install.
pub struct Plan<'a> {
    steps: &'a [InstallStep],
    project: &'a str,
    project_root: &'a Path,
    /// The working directory; `run` commands start here.
    build_root: &'a Path,
    home: Option<Home>,
}

/// The per-user side: `$HOME`, brokkr's state directory and the registry in
/// it. Absent when `$HOME` is unset, which only a copy step needs.
struct Home {
    home: PathBuf,
    state_dir: PathBuf,
    registry: Registry,
}

impl Home {
    fn from_env() -> Option<Self> {
        let home = crate::user_dirs::home()?;
        let state_dir = crate::user_dirs::brokkr_state_dir()?;
        // Anchored at the home as spelled, the spelling the registry path
        // is derived from.
        let registry = Registry::at(home.clone(), crate::user_dirs::install_registry()?);
        let home = std::fs::canonicalize(&home).unwrap_or(home);
        let state_dir = std::fs::canonicalize(&state_dir).unwrap_or(state_dir);
        Some(Self {
            home,
            state_dir,
            registry,
        })
    }
}

/// A copy destination resolved against the filesystem: the canonical parent
/// (so a symlinked `~/.claude` works) joined with the final name, which is
/// never followed.
struct Resolved {
    path: PathBuf,
    key: String,
    parent: PathBuf,
    name: OsString,
}

/// Whether a copy step may write its destination, and how.
enum Admission {
    /// Nothing there; `prior` is a stale record's owner, restored on abort.
    Fresh { prior: Option<Owner> },
    /// This step's own unchanged install, to be replaced.
    Replace { prior: Owner },
    /// This step's own unchanged install, already equal to the source.
    Unchanged,
}

/// A copy step that passed every check.
struct Admitted {
    resolved: Resolved,
    source: PathBuf,
    admission: Admission,
}

fn refused(step: &str, msg: &str) -> DevError {
    DevError::Refused(format!("install step '{step}': {msg}"))
}

/// Recover interrupted installs and check every step, before any package is
/// installed. `steps` is `None` in a tree with no `brokkr.toml`, which still
/// recovers: the registry is per user, not per project.
pub fn preflight<'a>(steps: Option<Steps<'a>>, build_root: &'a Path) -> Result<Plan<'a>, DevError> {
    preflight_with(steps, build_root, Home::from_env())
}

fn preflight_with<'a>(
    steps: Option<Steps<'a>>,
    build_root: &'a Path,
    home: Option<Home>,
) -> Result<Plan<'a>, DevError> {
    let plan = match steps {
        Some(s) => Plan {
            steps: s.steps,
            project: s.project,
            project_root: s.project_root,
            build_root,
            home,
        },
        None => Plan {
            steps: &[],
            project: "",
            project_root: build_root,
            build_root,
            home,
        },
    };
    if let Some(home) = &plan.home {
        transaction::recover_all(&home.registry)?;
    }

    let mut destinations: Vec<(&str, PathBuf)> = Vec::new();
    let mut after_run = false;
    for step in plan.steps {
        let (from, to) = match &step.action {
            InstallAction::Run { .. } => {
                after_run = true;
                continue;
            }
            InstallAction::Copy { from, to } => (from, to),
        };
        let resolved = plan.admit(&step.name, from, to, after_run, false)?.resolved;
        if let Some((other, _)) = destinations
            .iter()
            .find(|(_, d)| d.starts_with(&resolved.path) || resolved.path.starts_with(d))
        {
            return Err(refused(
                &step.name,
                &format!(
                    "{} overlaps the destination of step '{other}'",
                    resolved.path.display()
                ),
            ));
        }
        destinations.push((&step.name, resolved.path));
    }
    Ok(plan)
}

impl Plan<'_> {
    fn home(&self, step: &str) -> Result<&Home, DevError> {
        self.home.as_ref().ok_or_else(|| {
            refused(
                step,
                "HOME is not set, so there is no registry to record ownership in",
            )
        })
    }

    /// Run every step in order. The first failure stops the rest.
    pub fn execute(&self, lock: Option<&crate::lockfile::LockGuard>) -> Result<(), DevError> {
        for step in self.steps {
            match &step.action {
                InstallAction::Run { command } => self.run(&step.name, command, lock)?,
                InstallAction::Copy { from, to } => self.copy(&step.name, from, to)?,
            }
        }
        Ok(())
    }

    fn run(
        &self,
        step: &str,
        command: &str,
        lock: Option<&crate::lockfile::LockGuard>,
    ) -> Result<(), DevError> {
        output::install_msg(&format!("{step}: sh -c {command}"));
        let out = output::run_passthrough_in(
            "sh",
            &["-c", command],
            Some(self.build_root),
            &[],
            output::SpawnProfile::Plain,
            lock,
        )
        .map_err(|e| match e {
            DevError::Interrupted => e,
            other => DevError::Refused(format!("install step '{step}' failed: {other}")),
        })?;
        match out.failed_code(&[]) {
            // The command printed its own output; this names the step.
            Some(code) => Err(DevError::Reported(format!(
                "install step '{step}' failed (exit code {code})"
            ))),
            None => Ok(()),
        }
    }

    fn copy(&self, step: &str, from: &str, to: &str) -> Result<(), DevError> {
        output::install_msg(&format!("{step}: copy {from} -> {to}"));
        let Admitted {
            resolved,
            source,
            admission,
        } = self.admit(step, from, to, false, true)?;
        let home = self.home(step)?;
        let (prior, displaced) = match admission {
            Admission::Unchanged => {
                output::install_msg(&format!("{step}: already up to date"));
                return Ok(());
            }
            Admission::Fresh { prior } => (prior, None),
            Admission::Replace { prior } => {
                let displaced = prior.fingerprint.clone();
                (Some(prior), Some(displaced))
            }
        };
        let target = transaction::Target {
            path: &resolved.path,
            key: &resolved.key,
            parent: &resolved.parent,
            name: &resolved.name,
        };
        let owner_of = |fingerprint| Owner {
            project: self.project.to_owned(),
            step: step.to_owned(),
            fingerprint,
        };
        transaction::install(
            &home.registry,
            step,
            &target,
            &source,
            owner_of,
            prior.as_ref(),
            displaced.as_deref(),
        )
    }

    /// Resolve and admit one copy step. `source_may_follow` lets a missing
    /// source pass (preflight, after a `run` step that may generate it). With
    /// `create`, every check passes on the uncreated path first, then the
    /// destination's missing parents are created and every check runs again
    /// on the created one - so a refused step creates nothing.
    fn admit(
        &self,
        step: &str,
        from: &str,
        to: &str,
        source_may_follow: bool,
        create: bool,
    ) -> Result<Admitted, DevError> {
        let home = self.home(step)?;
        let source = self.project_root.join(from);
        let source_fingerprint = match tree::observe(&source) {
            Ok(Observed::Tree(fingerprint)) => Some(fingerprint),
            Ok(Observed::Absent) if source_may_follow => None,
            Ok(Observed::Absent) => {
                return Err(refused(
                    step,
                    &format!("copy.from {} does not exist", source.display()),
                ));
            }
            Ok(Observed::Foreign(path)) => {
                return Err(refused(
                    step,
                    &format!(
                        "{} is a symlink or special file; a copy payload holds regular files and directories only",
                        path.display()
                    ),
                ));
            }
            Err(e) => {
                return Err(refused(
                    step,
                    &format!("cannot read {}: {e}", source.display()),
                ));
            }
        };
        let source = std::fs::canonicalize(&source).unwrap_or(source);

        let check = |create: bool| -> Result<(Resolved, Admission), DevError> {
            let resolved = resolve(to, &home.home, create).map_err(|msg| refused(step, &msg))?;
            self.refuse_overlaps(step, &resolved, &source)?;
            let admission = self.ownership(step, &resolved, source_fingerprint.as_deref())?;
            Ok((resolved, admission))
        };
        let (mut resolved, mut admission) = check(false)?;
        if create {
            (resolved, admission) = check(true)?;
        }
        Ok(Admitted {
            resolved,
            source,
            admission,
        })
    }

    /// Whether this step may write `dest`. Ownership is settled from the
    /// registry and the destination's existence before anything is read:
    /// only this step's own install is ever fingerprinted, so a `to` that
    /// names someone else's large directory is refused without walking it.
    fn ownership(
        &self,
        step: &str,
        dest: &Resolved,
        source: Option<&str>,
    ) -> Result<Admission, DevError> {
        let home = self.home(step)?;
        let record_file = home.registry.file_for(&dest.key);
        let record = home
            .registry
            .load(&dest.key)
            .map_err(|e| refused(step, &e.to_string()))?;
        if record.as_ref().is_some_and(|r| r.pending.is_some()) {
            return Err(refused(
                step,
                &format!(
                    "an interrupted install of {} is unresolved (see the warning above, and the record {})",
                    dest.path.display(),
                    record_file.display()
                ),
            ));
        }
        let owner = record.and_then(|r| r.owner);
        match std::fs::symlink_metadata(&dest.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Admission::Fresh { prior: owner });
            }
            Err(e) => {
                return Err(refused(
                    step,
                    &format!("cannot inspect {}: {e}", dest.path.display()),
                ));
            }
            Ok(_) => {}
        }
        let owner = match owner {
            Some(owner) if owner.project == self.project && owner.step == step => owner,
            Some(other) => {
                return Err(refused(
                    step,
                    &format!(
                        "{} was installed by project {} step '{}', and brokkr replaces only what this step \
                         installed",
                        dest.path.display(),
                        other.project,
                        other.step
                    ),
                ));
            }
            None => {
                return Err(refused(
                    step,
                    &format!(
                        "{} exists and brokkr did not install it; brokkr replaces only what this step \
                         installed, so give copy.to a path of its own (or, if that path is an old copy of \
                         this artifact, remove it)",
                        dest.path.display()
                    ),
                ));
            }
        };
        match tree::observe(&dest.path) {
            Ok(Observed::Tree(live)) if live == owner.fingerprint => {
                if source == Some(live.as_str()) {
                    Ok(Admission::Unchanged)
                } else {
                    Ok(Admission::Replace { prior: owner })
                }
            }
            Ok(_) => Err(refused(
                step,
                &format!(
                    "{} was changed since brokkr installed it; remove it (or undo the change) to reinstall",
                    dest.path.display()
                ),
            )),
            Err(e) => Err(refused(
                step,
                &format!("cannot read {}: {e}", dest.path.display()),
            )),
        }
    }

    /// A destination brokkr may replace wholesale must not contain anything
    /// it has no business deleting: home, the project, the checkout, brokkr's
    /// own state, the source it copies from, or another step's artifact.
    fn refuse_overlaps(&self, step: &str, dest: &Resolved, source: &Path) -> Result<(), DevError> {
        let home = self.home(step)?;
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let protected = [
            ("your home directory", home.home.clone()),
            ("the project root", canonical(self.project_root)),
            ("the build root", canonical(self.build_root)),
            ("brokkr's state directory", home.state_dir.clone()),
        ];
        for (what, path) in &protected {
            if path.starts_with(&dest.path) {
                return Err(refused(
                    step,
                    &format!(
                        "{} contains {what} ({})",
                        dest.path.display(),
                        path.display()
                    ),
                ));
            }
        }
        if dest.path.starts_with(&home.state_dir) {
            return Err(refused(
                step,
                &format!("{} is inside brokkr's state directory", dest.path.display()),
            ));
        }
        if dest.path.starts_with(source) || source.starts_with(&dest.path) {
            return Err(refused(
                step,
                &format!(
                    "{} overlaps its own source {}",
                    dest.path.display(),
                    source.display()
                ),
            ));
        }
        let scan = home
            .registry
            .all()
            .map_err(|e| refused(step, &e.to_string()))?;
        if let Some(broken) = scan.broken.first() {
            return Err(refused(
                step,
                &format!(
                    "the install registry cannot be read whole, so a conflicting claim cannot be ruled out: {broken}"
                ),
            ));
        }
        for (_, record) in scan.records {
            if record.destination == dest.key {
                continue;
            }
            let other = Path::new(&record.destination);
            if !(other.starts_with(&dest.path) || dest.path.starts_with(other)) {
                continue;
            }
            // Only a destination that is really gone releases its claim; one
            // that cannot be inspected still holds it.
            let held = record.pending.is_some()
                || (record.owner.is_some()
                    && match std::fs::symlink_metadata(other) {
                        Ok(_) => true,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                        Err(e) => {
                            return Err(refused(
                                step,
                                &format!("cannot inspect {}: {e}", other.display()),
                            ));
                        }
                    });
            if held {
                let by = record.owner.as_ref().map_or_else(String::new, |o| {
                    format!(", installed by project {} step '{}'", o.project, o.step)
                });
                return Err(refused(
                    step,
                    &format!(
                        "{} nests with the managed destination {}{by}",
                        dest.path.display(),
                        other.display()
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// Resolve a `copy.to` (already lexically validated at config load) against
/// the filesystem. The nearest existing ancestor of the parent is
/// canonicalized and the missing rest appended; with `create`, the missing
/// directories are made durably (below `home`, the whole chain from it), and
/// the parent is canonicalized again.
fn resolve(to: &str, home: &Path, create: bool) -> Result<Resolved, String> {
    let expanded = match to.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(to),
    };
    let name = expanded
        .file_name()
        .ok_or_else(|| format!("copy.to {to:?} names no file"))?
        .to_owned();
    let lexical_parent = expanded
        .parent()
        .ok_or_else(|| format!("copy.to {to:?} has no parent"))?;
    let mut base = lexical_parent.to_path_buf();
    let mut missing: Vec<OsString> = Vec::new();
    loop {
        match std::fs::symlink_metadata(&base) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let component = base
                    .file_name()
                    .ok_or_else(|| format!("{} does not exist", base.display()))?
                    .to_owned();
                missing.push(component);
                base = base
                    .parent()
                    .ok_or_else(|| format!("{} does not exist", base.display()))?
                    .to_path_buf();
            }
            Err(e) => return Err(format!("cannot inspect {}: {e}", base.display())),
        }
    }
    let mut parent = std::fs::canonicalize(&base)
        .map_err(|e| format!("cannot resolve {}: {e}", base.display()))?;
    if !parent.is_dir() {
        return Err(format!("{} is not a directory", parent.display()));
    }
    // Below the home, every directory from it down to the parent is created
    // and synced on each creating call, so one an earlier install created and
    // died before syncing is made durable now, before any record names it.
    // Outside the home no directory is known to be durable, so only the ones
    // this call creates are synced: an install that created a parent outside
    // the home and died before its sync leaves that parent unsynced for every
    // retry, and a crash that then loses it leaves a journaled install whose
    // recorded parent is gone - refused by recovery, blocking its destination
    // until resolved by hand.
    let anchored = create && parent.starts_with(home);
    for component in missing.iter().rev() {
        let next = parent.join(component);
        if create && !anchored {
            match std::fs::create_dir(&next) {
                Ok(()) => tree::fsync_dir(&parent)
                    .map_err(|e| format!("cannot fsync {}: {e}", parent.display()))?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("cannot create {}: {e}", next.display())),
            }
        }
        parent = next;
    }
    if anchored {
        crate::atomic_write::create_dir_all_durable(home, &parent)
            .map_err(|e| format!("cannot create {} durably: {e}", parent.display()))?;
    }
    if create {
        parent = std::fs::canonicalize(&parent)
            .map_err(|e| format!("cannot resolve {}: {e}", parent.display()))?;
    }
    let path = parent.join(&name);
    let key = path
        .to_str()
        .ok_or_else(|| {
            format!(
                "{} is not UTF-8, which the install registry cannot record",
                path.display()
            )
        })?
        .to_owned();
    Ok(Resolved {
        path,
        key,
        parent,
        name,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    /// A project tree, a home directory and a registry, all under scratch.
    struct World {
        root: PathBuf,
        project: PathBuf,
        home: PathBuf,
    }

    fn world(name: &str) -> World {
        let root =
            std::fs::canonicalize(crate::test_scratch::scratch("install_step", name)).unwrap();
        let project = root.join("project");
        std::fs::create_dir_all(project.join("mod")).unwrap();
        std::fs::write(project.join("mod/SKILL.md"), "v1").unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(home.join("state")).unwrap();
        World {
            root,
            project,
            home,
        }
    }

    impl World {
        /// The test home keeps its brokkr state in `~/state`.
        fn home(&self) -> Home {
            Home {
                home: self.home.clone(),
                state_dir: self.home.join("state"),
                registry: Registry::at(self.home.clone(), self.home.join("state/install")),
            }
        }

        fn install(&self, project: &str, steps: &[InstallStep]) -> Result<(), DevError> {
            let s = Steps {
                steps,
                project,
                project_root: &self.project,
            };
            preflight_with(Some(s), &self.project, Some(self.home()))?.execute(None)
        }
    }

    fn copy(name: &str, from: &str, to: &str) -> InstallStep {
        InstallStep {
            name: name.into(),
            action: InstallAction::Copy {
                from: from.into(),
                to: to.into(),
            },
        }
    }

    fn message(result: Result<(), DevError>) -> String {
        match result {
            Ok(()) => panic!("expected a refusal"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_tilde_destination_resolves_through_a_symlinked_parent() {
        let root = crate::test_scratch::scratch("install_step", "resolve_symlink");
        let real = root.join("dotfiles/claude");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, root.join(".claude")).unwrap();
        let resolved = resolve("~/.claude/skills/mod", &root, false).unwrap();
        let canonical_real = std::fs::canonicalize(&real).unwrap();
        assert_eq!(resolved.path, canonical_real.join("skills/mod"));
        assert!(
            !canonical_real.join("skills").exists(),
            "resolution alone creates nothing"
        );
        let created = resolve("~/.claude/skills/mod", &root, true).unwrap();
        assert_eq!(created.path, resolved.path);
        assert!(canonical_real.join("skills").is_dir());
    }

    #[test]
    fn an_owned_destination_is_replaced_kept_when_unchanged_and_refused_when_edited() {
        let w = world("owned_lifecycle");
        let step = [copy("mod", "mod", "~/skills/mod")];
        w.install("p", &step).unwrap();
        let dest = w.home.join("skills/mod");
        assert_eq!(
            std::fs::read_to_string(dest.join("SKILL.md")).unwrap(),
            "v1"
        );

        // Unchanged: nothing is staged, so no container ever appears.
        w.install("p", &step).unwrap();

        std::fs::write(w.project.join("mod/SKILL.md"), "v2").unwrap();
        w.install("p", &step).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("SKILL.md")).unwrap(),
            "v2"
        );
        assert_eq!(std::fs::read_dir(w.home.join("skills")).unwrap().count(), 1);

        std::fs::write(dest.join("mine.md"), "hand edit").unwrap();
        std::fs::write(w.project.join("mod/SKILL.md"), "v3").unwrap();
        assert!(message(w.install("p", &step)).contains("changed since brokkr installed it"));
        assert_eq!(
            std::fs::read_to_string(dest.join("mine.md")).unwrap(),
            "hand edit"
        );
    }

    #[test]
    fn a_destination_brokkr_did_not_install_or_another_step_owns_is_refused() {
        let w = world("foreign_destinations");
        std::fs::create_dir_all(w.home.join("theirs")).unwrap();
        std::fs::write(w.home.join("theirs/data"), "keep").unwrap();
        let msg = message(w.install("p", &[copy("mod", "mod", "~/theirs")]));
        assert!(msg.contains("brokkr did not install it"), "{msg}");
        assert!(w.home.join("theirs/data").exists());

        w.install("p", &[copy("a", "mod", "~/shared")]).unwrap();
        let msg = message(w.install("p", &[copy("b", "mod", "~/shared")]));
        assert!(msg.contains("installed by project p step 'a'"), "{msg}");
        let msg = message(w.install("q", &[copy("a", "mod", "~/shared")]));
        assert!(msg.contains("installed by project p step 'a'"), "{msg}");
    }

    #[test]
    fn protected_paths_overlaps_and_nesting_are_refused_before_anything_is_created() {
        let w = world("protected");
        let refuse = |to: &str, want: &str| {
            let msg = message(w.install("p", &[copy("mod", "mod", to)]));
            assert!(msg.contains(want), "{to}: {msg}");
        };
        refuse(w.root.to_str().unwrap(), "contains your home directory");
        refuse(
            w.project.join("mod/inner").to_str().unwrap(),
            "overlaps its own source",
        );
        refuse("~/state/x", "inside brokkr's state directory");

        let two = [copy("a", "mod", "~/out/a"), copy("b", "mod", "~/out/a/b")];
        assert!(message(w.install("p", &two)).contains("overlaps the destination of step 'a'"));
        assert!(
            !w.home.join("out").exists(),
            "a refused preflight creates no parent"
        );

        w.install("p", &[copy("a", "mod", "~/out/a")]).unwrap();
        refuse("~/out", "nests with the managed destination");
        refuse("~/out/a/deeper", "nests with the managed destination");
    }

    #[test]
    fn a_source_a_run_step_generates_may_be_missing_at_preflight_only() {
        let w = world("generated_source");
        let steps = [
            InstallStep {
                name: "gen".into(),
                action: InstallAction::Run {
                    command: "mkdir -p gen && echo x > gen/f".into(),
                },
            },
            copy("pub", "gen", "~/gen"),
        ];
        let s = Steps {
            steps: &steps,
            project: "p",
            project_root: &w.project,
        };
        preflight_with(Some(s), &w.project, Some(w.home())).unwrap();
        let missing = [copy("pub", "gen", "~/gen")];
        assert!(message(w.install("p", &missing)).contains("does not exist"));
    }

    #[test]
    fn an_unreadable_registry_file_refuses_copy_steps_only() {
        let w = world("broken_registry");
        std::fs::create_dir_all(w.home.join("state/install")).unwrap();
        std::fs::write(w.home.join("state/install/0000.json"), "{").unwrap();
        let msg = message(w.install("p", &[copy("mod", "mod", "~/m")]));
        assert!(msg.contains("cannot be read whole"), "{msg}");
        w.install("p", &[]).unwrap();
    }
}
