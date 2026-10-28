// What `brokkr check` runs concurrently, and the boundaries that keep it sound.
//
// Two kinds of concurrency, both bounded by one rule: no compilation overlaps
// the execution of a test binary (a listing included). The per-test hard cap
// and the listing deadline are wall-clock limits, so a compile running beside
// them would turn a timing limit into a load-dependent verdict.
//
// - Background script checks (`stage = "background"`): started with the
//   convention phases, joined before the prepare phase lists anything (or at
//   the end of the run when nothing is listed). Opt-in per entry, because an
//   opaque command's independence from what the run reads and writes cannot be
//   inferred.
// - Target-dir groups: the cargo invocations of clippy, rustdoc and the
//   prepare phase's builds, grouped by the directory they write. Inside a group
//   the work stays serial in configured order - cargo would serialise on the
//   build-dir lock anyway, and the order decides which in-place uplift lands
//   last. Different groups run on their own threads. The guarantees cover
//   managed, awaited work: a detached descendant of a build script or listing
//   is outside them.

/// Decide, for this run, whether the build phases collapse every target-dir
/// group into one chain: `[test] serial_builds`, or a configured build dir
/// (`CARGO_BUILD_BUILD_DIR` - inherited or in a sweep's own `env` - or
/// `build.build-dir` anywhere in the config chain), which holds intermediate
/// artifacts that target dirs do not separate. Resolved once per run for the
/// lanes of the phases it schedules - `check` asks once for clippy and rustdoc
/// over every lane and once for preparation over the tested lanes, so a lint
/// lane's env never reschedules a test build - and passed to every
/// [`run_grouped`] call of those phases. `phases` names them in the log line.
fn resolve_serial_builds(
    project_root: &Path,
    configured: bool,
    sweeps: &[PlannedSweep],
    phases: &str,
    narrate: &Narrator,
) -> bool {
    let sweep_probes: Vec<(bool, bool)> = sweeps.iter().map(|sweep| {
        let env: Vec<(&str, &str)> = sweep
            .env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        build_dir_configured(
            project_root,
            crate::rustflags::EffectiveEnv::new(&env),
        )
    }).collect();
    let default_probe = sweeps.is_empty().then(|| build_dir_configured(
            project_root,
            crate::rustflags::EffectiveEnv::new(&[]),
        ));
    let sweep_build_dir = sweep_probes.iter().any(|(configured, _)| *configured);
    let config_unreadable = sweep_probes.iter().any(|(_, unreadable)| *unreadable)
        || default_probe.is_some_and(|(_, unreadable)| unreadable);
    let default_build_dir = default_probe.is_some_and(|(configured, _)| configured);
    let serial = configured || sweep_build_dir || default_build_dir;
    if serial && !configured {
        let reason = if config_unreadable {
            "Cargo config could not be fully inspected"
        } else {
            "a cargo build dir is configured"
        };
        narrate.detail(&format!("{phases}: one cargo chain ({reason})"));
    }
    serial
}

fn build_dir_configured(
    project_root: &Path,
    env: crate::rustflags::EffectiveEnv<'_>,
) -> (bool, bool) {
    match crate::rustflags::CargoLaunchConfig::discover(project_root, env) {
        Ok(config) => (config.build_dir, false),
        // This probe only chooses whether build groups may overlap. An
        // incomplete config view must choose the serial path; the Cargo
        // invocation that follows remains responsible for reporting its own
        // unreadable or invalid config file.
        Err(_) => (true, true),
    }
}

/// The directory a sweep's cargo invocations write: its isolated target dir,
/// else the shared one. Canonicalized where it exists, so two spellings of
/// one directory are one group.
fn target_dir_key(sweep: &PlannedSweep, meta_target_dir: &Path) -> PathBuf {
    let dir = isolated_target_dir(sweep, meta_target_dir).unwrap_or_else(|| meta_target_dir.to_path_buf());
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

/// Run `work(i)` for every index in `items`: items sharing a key run one after
/// another in the order given, items with different keys on their own
/// threads. Every item runs whatever an earlier one returned - except after a
/// stop (`Interrupted`, or the shutdown flag), which ends its chain; the rest
/// of that chain is `None`. Results come back in the order of `items`.
/// `serial` (the run's [`resolve_serial_builds`] answer) makes every item one
/// chain.
fn run_grouped<T: Send>(
    items: &[(usize, PathBuf)],
    serial: bool,
    work: &(dyn Fn(usize) -> Result<T, DevError> + Sync),
) -> Vec<(usize, Option<Result<T, DevError>>)> {
    let mut chains: Vec<(PathBuf, Vec<usize>)> = Vec::new();
    for (pos, (_, key)) in items.iter().enumerate() {
        match chains.iter_mut().find(|(k, _)| serial || k == key) {
            Some((_, chain)) => chain.push(pos),
            None => chains.push((key.clone(), vec![pos])),
        }
    }
    let mut slots: Vec<Option<Result<T, DevError>>> = items.iter().map(|_| None).collect();
    let run_chain = |chain: &[usize]| -> Vec<(usize, Result<T, DevError>)> {
        let mut out = Vec::new();
        for &pos in chain {
            let r = work(items[pos].0);
            let stopped = matches!(r, Err(DevError::Interrupted)) || crate::shutdown::is_shutdown_requested();
            out.push((pos, r));
            if stopped {
                break;
            }
        }
        out
    };
    if chains.len() <= 1 {
        for (_, chain) in &chains {
            for (pos, r) in run_chain(chain) {
                slots[pos] = Some(r);
            }
        }
    } else {
        std::thread::scope(|s| {
            let handles: Vec<_> = chains.iter().map(|(_, chain)| s.spawn(|| run_chain(chain))).collect();
            for h in handles {
                match h.join() {
                    Ok(results) => {
                        for (pos, r) in results {
                            slots[pos] = Some(r);
                        }
                    }
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
        });
    }
    items.iter().map(|(i, _)| *i).zip(slots).collect()
}

/// The background script checks of one run.
struct BackgroundScripts {
    state: std::sync::Mutex<BackgroundState>,
}

enum BackgroundState {
    /// Nothing to join: no background entry, or the stage was skipped.
    Inert,
    Running(Vec<(ScriptCheck, std::thread::JoinHandle<Result<crate::script_check::Outcome, DevError>>)>),
    /// Joined and reported; the verdict a second join returns.
    Joined(Result<(), DevError>),
}

impl BackgroundScripts {
    /// Start every `stage = "background"` entry, each on its own thread, each
    /// with the script_check phase ceiling measured from its own start.
    fn start(project_root: &Path, checks: &[ScriptCheck], skipped: bool) -> Self {
        let entries: Vec<&ScriptCheck> = checks.iter().filter(|c| c.stage == Stage::Background).collect();
        if skipped || entries.is_empty() {
            return Self { state: std::sync::Mutex::new(BackgroundState::Inert) };
        }
        output::detail(&format!(
            "script-check: {} started in the background",
            output::count(entries.len(), "check")
        ));
        let deadline = Phase::ScriptCheck.ceiling();
        let running = entries
            .into_iter()
            .map(|check| {
                let (owned, root) = (check.clone(), project_root.to_path_buf());
                let handle =
                    std::thread::spawn(move || crate::script_check::run_one(&owned, &root, deadline));
                (check.clone(), handle)
            })
            .collect();
        Self { state: std::sync::Mutex::new(BackgroundState::Running(running)) }
    }

    /// Wait for every entry and report the stage once: its green line, or its
    /// failure block. Later calls return the same verdict without printing.
    fn join(&self) -> Result<(), DevError> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let running = match std::mem::replace(&mut *state, BackgroundState::Inert) {
            BackgroundState::Inert => return Ok(()),
            BackgroundState::Joined(verdict) => {
                let again = clone_verdict(&verdict);
                *state = BackgroundState::Joined(verdict);
                return again;
            }
            BackgroundState::Running(running) => running,
        };
        let total = running.len();
        let mut checks: Vec<ScriptCheck> = Vec::new();
        let mut outcomes: Vec<crate::script_check::Outcome> = Vec::new();
        // Entries that could not be run at all (a spawn failure, a panic),
        // each with its reason; an interrupt is the run's, not an entry's.
        let mut unrun: Vec<String> = Vec::new();
        let mut interrupted = false;
        for (check, handle) in running {
            match handle.join() {
                Ok(Ok(outcome)) => {
                    checks.push(check);
                    outcomes.push(outcome);
                }
                Ok(Err(DevError::Interrupted)) => interrupted = true,
                Ok(Err(e)) => unrun.push(format!("{}: could not be run: {e}", check.name)),
                Err(_) => unrun.push(format!("{}: its runner panicked", check.name)),
            }
        }
        let verdict = if interrupted {
            Err(DevError::Interrupted)
        } else {
            // Every entry that ran is reported, failures with their output,
            // whatever happened to the others.
            let failures: Vec<(&ScriptCheck, crate::script_check::Outcome)> =
                checks.iter().zip(outcomes).filter(|(_, o)| !o.passed).collect();
            let ran = if unrun.is_empty() || !failures.is_empty() {
                report_script_stage(total - unrun.len(), &failures, "background check")
            } else {
                Ok(())
            };
            if unrun.is_empty() {
                ran
            } else {
                output::error(&format!("script-check: {}", unrun.join("\n")));
                Err(DevError::Reported("script-check failed".into()))
            }
        };
        *state = BackgroundState::Joined(clone_verdict(&verdict));
        verdict
    }

    /// Whether the joined stage failed. False before the join.
    fn failed(&self) -> bool {
        matches!(
            &*self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
            BackgroundState::Joined(Err(_))
        )
    }
}

/// The join for an exit that never reached one: an unwinding panic. Nothing is
/// reported - the run is already failing - but no entry outlives the run that
/// started it.
impl Drop for BackgroundScripts {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let BackgroundState::Running(running) = std::mem::replace(state, BackgroundState::Inert) {
            for (_, handle) in running {
                handle.join().ok();
            }
        }
    }
}

#[cfg(test)]
mod concurrency_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Items sharing a key run in the order given, an error does not stop
    /// its chain, a stop does (and only its own), and results come back in
    /// item order.
    #[test]
    fn grouped_chains_keep_order_and_only_a_stop_ends_one() {
        let items: Vec<(usize, PathBuf)> = vec![
            (0, "a".into()),
            (1, "b".into()),
            (2, "a".into()),
            (3, "b".into()),
            (4, "a".into()),
            (5, "b".into()),
        ];
        let order = std::sync::Mutex::new(Vec::new());
        let out = run_grouped(&items, false, &|i| {
            order.lock().unwrap().push(i);
            match i {
                2 => Err(DevError::Build("two".into())),
                3 => Err(DevError::Interrupted),
                _ => Ok(i * 10),
            }
        });
        let got: Vec<(usize, Option<bool>)> = out.iter().map(|(i, r)| (*i, r.as_ref().map(Result::is_ok))).collect();
        assert_eq!(
            got,
            vec![(0, Some(true)), (1, Some(true)), (2, Some(false)), (3, Some(false)), (4, Some(true)), (5, None)]
        );
        let order = order.into_inner().unwrap();
        let a: Vec<usize> = order.iter().copied().filter(|i| i % 2 == 0).collect();
        assert_eq!(a, vec![0, 2, 4], "chain `a` ran in order, past its error");
    }

    /// A serial run is one chain: a stop anywhere ends every later item.
    #[test]
    fn serial_builds_make_one_chain() {
        let items: Vec<(usize, PathBuf)> = vec![(0, "a".into()), (1, "b".into()), (2, "a".into())];
        let out = run_grouped(&items, true, &|i| {
            if i == 0 { Err(DevError::Interrupted) } else { Ok(i) }
        });
        let got: Vec<(usize, bool)> = out.iter().map(|(i, r)| (*i, r.is_some())).collect();
        assert_eq!(got, vec![(0, true), (1, false), (2, false)]);
    }

    /// A snapshot names exactly the files whose content moved.
    #[test]
    fn snapshot_drift_names_changed_and_missing_files() {
        let dir = crate::test_scratch::scratch("concurrency", "snapshot_drift");
        let (same, changed, gone) = (dir.join("same"), dir.join("changed"), dir.join("gone"));
        for p in [&same, &changed, &gone] {
            std::fs::write(p, b"before").unwrap();
        }
        let snap = take_snapshot([&same, &changed, &gone].map(|p| p.display().to_string())).unwrap();
        assert!(snapshot_drift(&snap).is_empty());
        std::fs::write(&changed, b"after").unwrap();
        std::fs::remove_file(&gone).unwrap();
        let drift = snapshot_drift(&snap);
        assert_eq!(drift.len(), 2, "{drift:?}");
        assert!(drift.iter().any(|l| l.contains("changed") && l.contains("other content")));
        assert!(drift.iter().any(|l| l.contains("gone") && l.contains("could not be read")));
    }
}

/// A joined verdict, for a second caller. A failure was printed by the first
/// join, so the copy is `Reported`; an interrupt stays an interrupt.
fn clone_verdict(v: &Result<(), DevError>) -> Result<(), DevError> {
    match v {
        Ok(()) => Ok(()),
        Err(DevError::Interrupted) => Err(DevError::Interrupted),
        Err(_) => Err(DevError::Reported("script-check failed".into())),
    }
}
