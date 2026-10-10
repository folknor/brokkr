// The diagnostic continuation report: what a run left unresolved, and the
// command that runs those executions again.
//
// A run stopped early - a watchdog kill, a per-test timeout, an interrupt, a
// fail-fast - leaves executions that never reached a verdict. The report names
// every killed, hung, interrupted or otherwise unresolved one (no cap),
// grouped lane -> resolution -> binary, with its outcome and why, and prints
// the exact command that replays them. The one exception is the plain
// fail-fast casualty (never started because an earlier failure stopped the
// run): the text counts those per binary, since listing them buries the
// failure; `--json` and `--from-run ID --list` still name each. It is
// built from the plan and the journal alone, by the same reconciliation the
// audit uses, so it can be rebuilt later from disk (`brokkr test --from-run ID
// --list`) when a hard exit stopped the original run from printing it.
//
// A DIAGNOSTIC and nothing else. It certifies nothing, it never reruns
// anything by itself, and a failed run never turns green because of it. The
// words it uses mean exactly this and no more:
//
// timed_out means the execution exceeded its attributed budget, not that it
// is proven to be the cause of a hang. Interrupted means a start was observed
// without an acceptable terminal. Unobserved means no start and no terminal
// were recorded: it may have run if records were lost, and missing evidence is
// not proof of non-execution. A continuation that passes means those
// executions passed in a NEW process against the CURRENT external state; the
// original run stays failed, and a replay restores neither the process history
// (globals earlier tests initialised) nor the external state the killed tests
// left behind.

/// The statement every report makes about itself, in the `--json` trailer
/// and (in prose) in the text.
const CONTINUATION_STATEMENT: &str = "diagnostic only: a continuation runs the listed executions again in a new \
     process against the current state of the machine; it certifies nothing, never changes the verdict of \
     the run it continues, and restores neither process history nor the external state the killed tests left";

/// What a replay's environment is, stated wherever the command is.
const REPLAY_ENVIRONMENT: &str = "the recorded environment additions are applied over the ambient environment of \
     the replaying process; this is not a snapshot of the original environment (a nextest lane's \
     cargo-configuration environment and target runner are held to a recorded fingerprint, and the \
     replay refuses if they changed)";

/// One execution a run left unresolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) id: ExecutionId,
    pub(crate) outcome: Outcome,
    pub(crate) detail: Option<Detail>,
}

/// Every execution the reconciliation left `interrupted` or `unobserved`, in
/// the plan's order. Failed, timed-out, passed and ignored executions stand
/// as the results they are.
pub(crate) fn candidates_of(recon: &Reconciliation) -> Vec<Candidate> {
    recon
        .accounted
        .iter()
        .filter(|a| matches!(a.outcome, Outcome::Interrupted | Outcome::Unobserved))
        .map(|a| Candidate { id: a.id.clone(), outcome: a.outcome, detail: a.detail })
        .collect()
}

/// Read `journal` back (when the plan was persisted at all) and reconcile
/// `plan` against it. Pure: spawns nothing, builds nothing, arms no deadline -
/// which is what lets it run after a watchdog kill, and from a later
/// invocation that only has the files.
pub(crate) fn reconcile_run(plan: &AccountingPlan, journal: Option<&Path>) -> (Vec<JournalRecord>, Reconciliation) {
    let (records, closed, mut errors) = match journal {
        Some(path) => read_journal(path),
        None => (Vec::new(), false, vec!["the plan was never persisted, so no journal exists".into()]),
    };
    if journal_unlocked_write_failed() {
        errors.push("the watchdog's deadline record could not be written whole".into());
    }
    let recon = reconcile(plan, &records, closed, errors);
    (records, recon)
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct CandidateOut {
    pub(crate) lane: String,
    pub(crate) resolution: Option<String>,
    pub(crate) package: String,
    pub(crate) binary: String,
    pub(crate) test: String,
    pub(crate) outcome: &'static str,
    pub(crate) detail: Option<&'static str>,
    /// The execution's full identity, as a replay selects it.
    pub(crate) execution: ExecutionId,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct LaneInventoryOut {
    pub(crate) lane: usize,
    pub(crate) label: String,
    pub(crate) kind: LaneKind,
    /// `complete`, `partial` (some of its binaries answer no libtest
    /// listing), `unavailable`, `skipped`, or `doctests` (a doctest-only lane
    /// has no enumerable inventory, by nature).
    pub(crate) availability: &'static str,
    pub(crate) reason: Option<String>,
    pub(crate) expected_executions: usize,
}

/// A test name a lane whose streams cannot be attributed to binaries blamed
/// at a stop: a suspect, named by the shared stream, with no binary.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SuspectOut {
    pub(crate) lane: String,
    pub(crate) test: String,
    pub(crate) cause: &'static str,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct InventoryOut {
    /// Enumerable binary tests only; doctests have no inventory.
    pub(crate) scope: &'static str,
    /// `complete` (every enumerable lane has an inventory and the plan is
    /// whole), `partial`, or `unavailable` (no lane has one).
    pub(crate) availability: &'static str,
    pub(crate) plan_complete: bool,
    pub(crate) journal_closed: bool,
    pub(crate) expected_executions: usize,
    pub(crate) unresolved: usize,
    pub(crate) anomalies: usize,
    pub(crate) lanes: Vec<LaneInventoryOut>,
    pub(crate) suspects: Vec<SuspectOut>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct CommandOut {
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: String,
    /// The argv shell-quoted, for a human to paste.
    pub(crate) display: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ReplayOut {
    pub(crate) available: bool,
    /// Why not, concretely, when not.
    pub(crate) refusals: Vec<String>,
    pub(crate) command: Option<CommandOut>,
    pub(crate) environment: &'static str,
}

/// The continuation report, as the `--json` trailer carries it.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ContinuationReport {
    pub(crate) source_run_id: String,
    /// Always false: nothing in a continuation certifies anything.
    pub(crate) certifies: bool,
    pub(crate) statement: &'static str,
    pub(crate) inventory: InventoryOut,
    pub(crate) candidates: Vec<CandidateOut>,
    pub(crate) replay: ReplayOut,
    /// The (lane, resolution, binary) groups of which something was observed:
    /// a start, a terminal, anything but silence. A group missing from it was
    /// never reached; the report says so. Not part of the trailer.
    #[serde(skip)]
    pub(crate) reached: Vec<(usize, Option<String>, BinaryUnit)>,
}

/// Quote one argument for a POSIX shell: bare when it is made of characters
/// that need no quoting, else in single quotes.
pub(crate) fn shell_quote(arg: &str) -> String {
    let bare = !arg.is_empty()
        && arg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | '@' | '%' | '+' | ','));
    if bare {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// `argv` as one pasteable line.
pub(crate) fn shell_display(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

/// The command that replays run `run_id`.
pub(crate) fn continuation_argv(run_id: &str) -> Vec<String> {
    ["brokkr", "test", "--from-run", run_id].map(str::to_owned).to_vec()
}

/// Group the candidates by lane, resolution and binary, in plan order, as the
/// replay selects them.
pub(crate) fn replay_groups(candidates: &[Candidate]) -> Vec<(usize, ReplayGroup)> {
    let mut out: Vec<(usize, ReplayGroup)> = Vec::new();
    for c in candidates {
        let p = &c.id.pair;
        match out
            .iter_mut()
            .rev()
            .find(|(lane, g)| *lane == c.id.lane && g.resolution == p.resolution && g.unit == p.unit)
        {
            Some((_, g)) => g.tests.push(p.test.clone()),
            None => out.push((
                c.id.lane,
                ReplayGroup { resolution: p.resolution.clone(), unit: p.unit.clone(), tests: vec![p.test.clone()] },
            )),
        }
    }
    out
}

fn lane_inventory(plan: &AccountingPlan, recon: &Reconciliation) -> Vec<LaneInventoryOut> {
    plan.lanes
        .iter()
        .map(|l| {
            let expected = recon.accounted.iter().filter(|a| a.id.lane == l.lane).count();
            let (availability, reason) = if let Some(why) = recon.superseded.get(&l.lane) {
                ("unavailable", Some(format!("the plan stopped describing this lane: {why}")))
            } else if let Some(why) = &l.skipped {
                ("skipped", Some(why.clone()))
            } else if let Some(why) = &l.unavailable {
                ("unavailable", Some(why.clone()))
            } else if l.kind == LaneKind::DocOnly {
                ("doctests", Some("doctests cannot be enumerated".to_owned()))
            } else if !l.unlisted.is_empty() {
                (
                    "partial",
                    Some(format!(
                        "{} cannot be enumerated, so the tests in {} are outside the inventory: {}",
                        output::count(l.unlisted.len(), "binary"),
                        if l.unlisted.len() == 1 { "it" } else { "them" },
                        l.unlisted.iter().map(|(id, why)| format!("{id} ({why})")).collect::<Vec<_>>().join("; ")
                    )),
                )
            } else {
                ("complete", None)
            };
            LaneInventoryOut {
                lane: l.lane,
                label: l.label.clone(),
                kind: l.kind,
                availability,
                reason,
                expected_executions: expected,
            }
        })
        .collect()
}

/// The tests a lane without an inventory blamed when it was stopped: the
/// per-test cap names a suspect on a shared stream. Named with no binary,
/// never turned into an execution, never replayable.
fn suspects(plan: &AccountingPlan, recon: &Reconciliation, records: &[JournalRecord]) -> Vec<SuspectOut> {
    let blind: BTreeSet<usize> = plan
        .lanes
        .iter()
        .filter(|l| l.unavailable.is_some())
        .map(|l| l.lane)
        .chain(recon.superseded.keys().copied())
        .collect();
    let mut out: Vec<SuspectOut> = Vec::new();
    for r in records {
        let JournalRecord::Terminated(t) = r else { continue };
        let (Some(lane), Some(test)) = (t.lane, &t.test) else { continue };
        if !blind.contains(&lane) {
            continue;
        }
        let label = plan.lane(lane).map_or_else(|| format!("lane {lane}"), |l| l.label.clone());
        let s = SuspectOut { lane: label, test: test.clone(), cause: t.cause.as_str() };
        if !out.iter().any(|o| o.lane == s.lane && o.test == s.test) {
            out.push(s);
        }
    }
    out
}

/// Build the report for run `plan.run_id`, or `None` when there is nothing to
/// continue: no unresolved execution and no suspect a blind lane named.
pub(crate) fn build_continuation(
    plan: &AccountingPlan,
    recon: &Reconciliation,
    records: &[JournalRecord],
) -> Option<ContinuationReport> {
    let cands = candidates_of(recon);
    let suspects = suspects(plan, recon, records);
    if cands.is_empty() && suspects.is_empty() {
        return None;
    }
    let lanes = lane_inventory(plan, recon);
    let enumerable: Vec<&LaneInventoryOut> = lanes
        .iter()
        .filter(|l| matches!(l.availability, "complete" | "partial" | "unavailable"))
        .collect();
    let available = enumerable.iter().filter(|l| l.availability != "unavailable").count();
    let availability = if enumerable.is_empty() || available == 0 {
        "unavailable"
    } else if enumerable.iter().all(|l| l.availability == "complete")
        && plan.complete
        && recon.superseded.is_empty()
    {
        "complete"
    } else {
        "partial"
    };

    let label_of = |lane: usize| plan.lane(lane).map_or_else(|| format!("lane {lane}"), |l| l.label.clone());
    let candidates: Vec<CandidateOut> = cands
        .iter()
        .map(|c| CandidateOut {
            lane: label_of(c.id.lane),
            resolution: c.id.pair.resolution.clone(),
            package: c.id.pair.unit.package.clone(),
            binary: format!("{}:{}", c.id.pair.unit.kind, c.id.pair.unit.target),
            test: c.id.pair.test.clone(),
            outcome: c.outcome.as_str(),
            detail: c.detail.map(Detail::as_str),
            execution: c.id.clone(),
        })
        .collect();

    let mut refusals: Vec<String> = Vec::new();
    let allocs = thread_allocations(records);
    for lane in plan.lanes.iter().filter(|l| cands.iter().any(|c| c.id.lane == l.lane)) {
        let groups: Vec<ReplayGroup> = replay_groups(&cands)
            .into_iter()
            .filter(|(l, _)| *l == lane.lane)
            .map(|(_, g)| g)
            .collect();
        refusals.extend(replay_refusals(lane, &groups, &allocs));
    }
    // A candidate in a lane the plan does not hold cannot be replayed from it.
    for lane in cands.iter().map(|c| c.id.lane).collect::<BTreeSet<_>>() {
        if plan.lane(lane).is_none() {
            refusals.push(format!("lane {lane} is not in the plan"));
        }
    }
    if cands.is_empty() {
        refusals.push(
            "no execution can be named: the lanes that stopped have no inventory, so the tests they \
             blamed are suspects of a shared stream, not executions of a binary"
                .into(),
        );
    }
    let replayable = refusals.is_empty() && !cands.is_empty();
    let cwd = plan.invocation.as_ref().map_or_else(|| ".".to_owned(), |i| i.cwd.clone());
    let command = replayable.then(|| {
        let argv = continuation_argv(&plan.run_id);
        CommandOut { display: shell_display(&argv), argv, cwd }
    });
    Some(ContinuationReport {
        source_run_id: plan.run_id.clone(),
        certifies: false,
        statement: CONTINUATION_STATEMENT,
        inventory: InventoryOut {
            scope: "binary_tests",
            availability,
            plan_complete: plan.complete,
            journal_closed: recon.journal_closed,
            expected_executions: recon.accounted.len(),
            unresolved: cands.len(),
            anomalies: recon.anomalies.len(),
            lanes,
            suspects,
        },
        candidates,
        replay: ReplayOut { available: replayable, refusals, command, environment: REPLAY_ENVIRONMENT },
        reached: recon
            .accounted
            .iter()
            .filter(|a| a.outcome != Outcome::Unobserved)
            .map(|a| (a.id.lane, a.id.pair.resolution.clone(), a.id.pair.unit.clone()))
            .collect(),
    })
}

/// The report as text, line by line (the caller prefixes them). Fail-fast
/// casualties are counted per binary, not listed; see
/// [`render_continuation_full`].
pub(crate) fn render_continuation(report: &ContinuationReport) -> Vec<String> {
    render_lines(report, true)
}

/// The report as text with every unresolved execution named, fail-fast
/// casualties included: what `brokkr test --from-run ID --list` prints.
pub(crate) fn render_continuation_full(report: &ContinuationReport) -> Vec<String> {
    render_lines(report, false)
}

fn render_lines(report: &ContinuationReport, collapse: bool) -> Vec<String> {
    let mut out = Vec::new();
    let n = report.candidates.len();
    out.push(format!(
        "diagnostic continuation: {n} unresolved {} (of {} expected)",
        if n == 1 { "execution" } else { "executions" },
        report.inventory.expected_executions
    ));

    // lane -> resolution -> binary, in the plan's order. A group none of whose
    // executions was ever seen was never reached: the binary a stop actually
    // killed is the one with an `interrupted` line.
    let mut groups: Vec<(String, Vec<&CandidateOut>)> = Vec::new();
    for c in &report.candidates {
        let key = match &c.resolution {
            Some(r) => format!("{} / {r} / {} / {}", c.lane, c.package, c.binary),
            None => format!("{} / {} / {}", c.lane, c.package, c.binary),
        };
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(c),
            None => groups.push((key, vec![c])),
        }
    }
    for (key, members) in &groups {
        let seen = members.iter().any(|c| {
            report.reached.iter().any(|(lane, resolution, unit)| {
                *lane == c.execution.lane
                    && *resolution == c.execution.pair.resolution
                    && *unit == c.execution.pair.unit
            })
        });
        // Fail-fast casualties are summarised by count: the failure that
        // stopped the run is already reported, and its casualties would bury
        // it. Every other unresolved execution (killed, hung, interrupted,
        // deadline) stays named; the full list is in `--json` and
        // `brokkr test --from-run ID --list`.
        let (casualties, named): (Vec<&&CandidateOut>, Vec<&&CandidateOut>) =
            members.iter().partition(|c| collapse && is_fail_fast_casualty(c));
        out.push(String::new());
        // A binary never reached at all folds into one line; one that ran
        // (its failure stopped the run) keeps its heading, so the count reads
        // as the rest of that binary, not as a binary that never started.
        if named.is_empty() && !seen {
            out.push(format!("{key}: not reached, {}", output::count(casualties.len(), "test")));
            continue;
        }
        out.push(if seen { key.clone() } else { format!("{key} (not reached)") });
        for c in named {
            let detail = c.detail.map_or_else(String::new, |d| format!("  {}", d.replace('_', " ")));
            out.push(format!("  {:<11}  {}{detail}", c.outcome, c.test));
        }
        if !casualties.is_empty() {
            out.push(format!("  not reached: {}", output::count(casualties.len(), "test")));
        }
    }

    for lane in report.inventory.lanes.iter().filter(|l| matches!(l.availability, "unavailable" | "partial")) {
        out.push(String::new());
        out.push(format!(
            "inventory {} for {}: {}",
            lane.availability,
            lane.label,
            lane.reason.as_deref().unwrap_or("not recorded")
        ));
        for s in report.inventory.suspects.iter().filter(|s| s.lane == lane.label) {
            out.push(format!(
                "  suspect      {}  {} (named by a shared stream; its binary is unknown, so it is not replayable)",
                s.test,
                s.cause.replace('_', " ")
            ));
        }
    }

    out.push(String::new());
    match &report.replay.command {
        Some(cmd) => {
            out.push("rerun these recorded executions:".to_owned());
            out.push(format!("  {}", cmd.display));
            out.push(format!("  (from {})", cmd.cwd));
        }
        None => {
            out.push("these executions cannot be rerun from the record:".to_owned());
            for r in &report.replay.refusals {
                out.push(format!("  {r}"));
            }
        }
    }
    out.push(String::new());
    // The pointer to `--from-run ID --list` is offered only when the record is
    // replayable: a lane with no attribution has nothing to rerun, and naming
    // a `--from-run` command there would read as a replay it cannot give.
    if collapse && report.replay.command.is_some() {
        out.push(format!(
            "note: diagnostic only - a rerun certifies nothing and never changes this run's verdict; \
             `brokkr test --from-run {} --list` shows the full statement and replay environment.",
            report.source_run_id
        ));
    } else {
        out.push(format!("note: {}.", report.statement));
        out.push(
            "note: an unobserved execution may have run if records were lost; missing evidence is not \
             proof of non-execution. A timed-out test exceeded its budget; that is not proof it caused a hang."
                .to_owned(),
        );
        if report.replay.command.is_some() {
            out.push(format!("note: {}.", report.replay.environment));
        }
    }
    out
}

/// An execution that never started only because an earlier failure stopped
/// the run (fail fast), as opposed to one a kill, hang or interrupt left.
fn is_fail_fast_casualty(c: &CandidateOut) -> bool {
    c.outcome == Outcome::Unobserved.as_str() && c.detail == Some(Detail::FailFast.as_str())
}

#[cfg(test)]
mod continuation_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn unit(target: &str) -> BinaryUnit {
        BinaryUnit {
            package_id: "path+file:///x/crate-a#crate-a@0.1.0".into(),
            package: "crate-a".into(),
            kind: "test".into(),
            target: target.into(),
        }
    }

    fn pair(u: &BinaryUnit, test: &str) -> PairId {
        PairId { shape: "s".into(), resolution: None, unit: u.clone(), test: test.into() }
    }

    fn obs(lane: usize, stream: u64, u: &BinaryUnit, event: ObsEvent) -> JournalRecord {
        JournalRecord::Observed {
            lane,
            stream,
            origin: StreamOrigin::Binary { resolution: None, unit: u.clone(), one_test: None },
            event,
        }
    }

    fn lane(idx: usize, label: &str, kind: LaneKind, tests: &[(&BinaryUnit, &str)]) -> LaneRecord {
        LaneRecord {
            prepared: true,
            executions: tests.iter().map(|(u, t)| pair(u, t)).collect(),
            replay: Some(test_recipe(tests.iter().map(|(u, _)| (*u).clone()).collect())),
            ..LaneRecord::empty(idx, label.into(), kind, "s".into())
        }
    }

    fn test_recipe(units: Vec<BinaryUnit>) -> LaneReplay {
        let mut seen: Vec<BinaryUnit> = Vec::new();
        for u in units {
            if !seen.contains(&u) {
                seen.push(u);
            }
        }
        LaneReplay {
            version: REPLAY_VERSION,
            refusal: None,
            model: ReplayModel::SerialShared,
            ignored_mode: IgnoredMode::Exclude,
            engine_launch: Vec::new(),
            test_threads: Some(1),
            parallel_budget: None,
            harness_args: Vec::new(),
            per_test_ceiling_secs: 20,
            env: Vec::new(),
            support_builds: Vec::new(),
            target_filters: Vec::new(),
            runtime_fingerprint: None,
            support_fingerprint: Vec::new(),
            resolutions: vec![ResolutionReplay {
                resolution: None,
                build_args: Vec::new(),
                binaries: seen
                    .iter()
                    .map(|u| BinaryReplay {
                        binary: TestBinary {
                            package: u.package.clone(),
                            package_id: u.package_id.clone(),
                            target: u.target.clone(),
                            kind: u.kind.clone(),
                            executable: format!("/t/debug/deps/{}-1", u.target),
                            manifest_dir: std::path::PathBuf::from("/x/crate-a"),
                        },
                        cwd: "/x/crate-a".into(),
                        env: Vec::new(),
                    })
                    .collect(),
            }],
        }
    }

    fn plan_of(lanes: Vec<LaneRecord>) -> AccountingPlan {
        AccountingPlan {
            run_id: "123456-2".into(),
            complete: true,
            lanes,
            invocation: Some(Invocation { cwd: "/x".into(), project_root: "/x".into() }),
            ..AccountingPlan::default()
        }
    }

    fn deadline() -> JournalRecord {
        JournalRecord::Terminated(Termination {
            scope: TerminationScope::Run,
            lane: None,
            stream: None,
            cause: TerminationCause::PhaseDeadline,
            test: None,
            charged: None,
        })
    }

    /// Validation case: a partial-profile serial lane killed mid-run names the
    /// interrupted test and the ones never reached, and prints a command.
    #[test]
    fn a_killed_lane_names_its_unresolved_tests_and_prints_a_command() {
        let u = unit("integration");
        let p = plan_of(vec![lane(
            0,
            "default",
            LaneKind::Serial,
            &[(&u, "detector::alpha"), (&u, "detector::beta"), (&u, "detector::gamma")],
        )]);
        let records = vec![
            obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 3 }),
            obs(0, 1, &u, ObsEvent::Started { name: "detector::alpha".into() }),
            deadline(),
        ];
        let recon = reconcile(&p, &records, false, Vec::new());
        let report = build_continuation(&p, &recon, &records).unwrap();
        assert_eq!(report.candidates.len(), 3);
        assert_eq!(report.candidates[0].outcome, "interrupted");
        assert_eq!(report.candidates[0].detail, Some("phase_deadline"));
        assert_eq!(report.candidates[1].outcome, "unobserved");
        assert!(report.replay.available, "{:?}", report.replay.refusals);
        let cmd = report.replay.command.as_ref().unwrap();
        assert_eq!(cmd.display, "brokkr test --from-run 123456-2");
        assert_eq!(cmd.cwd, "/x");
        assert!(!report.certifies);
        assert_eq!(report.inventory.availability, "complete");

        let text = render_continuation(&report).join("\n");
        assert!(text.contains("diagnostic continuation: 3 unresolved executions"), "{text}");
        assert!(text.contains("default / crate-a / test:integration\n"), "{text}");
        assert!(text.contains("  interrupted  detector::alpha  phase deadline"), "{text}");
        assert!(text.contains("  unobserved   detector::gamma  phase deadline"), "{text}");
        assert!(text.contains("rerun these recorded executions:\n  brokkr test --from-run 123456-2"), "{text}");
        assert!(text.contains("certifies nothing"), "{text}");
    }

    /// Validation case: a lane with no attribution reports its inventory
    /// unavailable and names no invented execution; only the suspect the
    /// shared stream blamed is mentioned, and it is not replayable.
    #[test]
    fn a_lane_with_no_attribution_reports_unavailable_instead_of_inventing_names() {
        let mut l = LaneRecord::empty(0, "threaded".into(), LaneKind::Serial, "s".into());
        l.unavailable = Some("cargo-mediated parallelism shares one stream".into());
        let p = plan_of(vec![l]);
        let records = vec![JournalRecord::Terminated(Termination {
            scope: TerminationScope::Lane,
            lane: Some(0),
            stream: Some(7),
            cause: TerminationCause::PerTestDeadline,
            test: Some("suite::hung".into()),
            charged: None,
        })];
        let recon = reconcile(&p, &records, false, Vec::new());
        let report = build_continuation(&p, &recon, &records).unwrap();
        assert!(report.candidates.is_empty());
        assert_eq!(report.inventory.availability, "unavailable");
        assert_eq!(report.inventory.suspects.len(), 1);
        assert_eq!(report.inventory.suspects[0].test, "suite::hung");
        assert!(!report.replay.available);
        assert!(report.replay.command.is_none());
        let text = render_continuation(&report).join("\n");
        assert!(text.contains("inventory unavailable for threaded"), "{text}");
        assert!(text.contains("suspect"), "{text}");
        assert!(!text.contains("brokkr test --from-run"), "{text}");
        assert!(text.contains(CONTINUATION_STATEMENT), "{text}");
        assert!(text.contains("missing evidence is not proof of non-execution"), "{text}");
        assert!(!text.contains(REPLAY_ENVIRONMENT), "{text}");
    }

    /// Validation case: the same test selected by two lanes is two candidates.
    #[test]
    fn the_same_test_in_two_lanes_is_two_candidates() {
        let u = unit("integration");
        let p = plan_of(vec![
            lane(0, "serial", LaneKind::Serial, &[(&u, "t::a")]),
            lane(1, "parallel", LaneKind::Parallel, &[(&u, "t::a")]),
        ]);
        let records = vec![deadline()];
        let recon = reconcile(&p, &records, false, Vec::new());
        let report = build_continuation(&p, &recon, &records).unwrap();
        assert_eq!(report.candidates.len(), 2);
        assert_ne!(report.candidates[0].execution, report.candidates[1].execution);
        assert_eq!(report.candidates[0].lane, "serial");
        assert_eq!(report.candidates[1].lane, "parallel");
        // Not deduplicated by name in the groups either.
        assert_eq!(replay_groups(&candidates_of(&recon)).len(), 2);
    }

    /// Failed, timed-out and passed executions stand as results; only the
    /// unresolved ones are candidates.
    #[test]
    fn only_interrupted_and_unobserved_are_candidates() {
        let u = unit("integration");
        let p = plan_of(vec![lane(
            0,
            "default",
            LaneKind::Serial,
            &[(&u, "a"), (&u, "b"), (&u, "c"), (&u, "d")],
        )]);
        let records = vec![
            obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 4 }),
            obs(0, 1, &u, ObsEvent::Started { name: "a".into() }),
            obs(0, 1, &u, ObsEvent::Finished { name: "a".into(), result: TestResult::Ok }),
            obs(0, 1, &u, ObsEvent::Started { name: "b".into() }),
            obs(0, 1, &u, ObsEvent::Finished { name: "b".into(), result: TestResult::Failed }),
            obs(0, 1, &u, ObsEvent::Started { name: "c".into() }),
            deadline(),
        ];
        let recon = reconcile(&p, &records, false, Vec::new());
        let cands = candidates_of(&recon);
        let names: Vec<&str> = cands.iter().map(|c| c.id.pair.test.as_str()).collect();
        assert_eq!(names, vec!["c", "d"]);
    }

    /// A run with nothing unresolved has no continuation.
    #[test]
    fn a_clean_run_has_nothing_to_continue() {
        let u = unit("integration");
        let p = plan_of(vec![lane(0, "default", LaneKind::Serial, &[(&u, "a")])]);
        let records = vec![
            obs(0, 1, &u, ObsEvent::SuiteStarted { test_count: 1 }),
            obs(0, 1, &u, ObsEvent::Started { name: "a".into() }),
            obs(0, 1, &u, ObsEvent::Finished { name: "a".into(), result: TestResult::Ok }),
        ];
        let recon = reconcile(&p, &records, true, Vec::new());
        assert!(build_continuation(&p, &recon, &records).is_none());
    }

    /// A lane whose recipe refuses makes the whole replay unavailable, with
    /// the lane's own reason, and no command is printed.
    #[test]
    fn a_refused_lane_withholds_the_command_and_says_why() {
        let u = unit("integration");
        let mut l = lane(0, "default", LaneKind::Serial, &[(&u, "a")]);
        if let Some(r) = &mut l.replay {
            r.refusal = Some("the launch environment of crate-a::integration was not recorded".into());
        }
        let p = plan_of(vec![l]);
        let recon = reconcile(&p, &[deadline()], false, Vec::new());
        let report = build_continuation(&p, &recon, &[deadline()]).unwrap();
        assert!(!report.replay.available);
        assert!(report.replay.refusals[0].contains("not recorded"), "{:?}", report.replay.refusals);
        assert!(report.replay.command.is_none());
    }

    /// A superseded lane's observations are not read, and the lane reads
    /// unavailable with the reason.
    #[test]
    fn a_superseded_lane_is_unavailable_with_its_reason() {
        let u = unit("integration");
        let p = plan_of(vec![
            lane(0, "first", LaneKind::Serial, &[(&u, "a")]),
            lane(1, "second", LaneKind::Parallel, &[(&u, "b")]),
        ]);
        let records = vec![
            JournalRecord::LaneSuperseded { lane: 1, reason: "artifacts changed since the plan".into() },
            obs(1, 3, &u, ObsEvent::Started { name: "other".into() }),
            deadline(),
        ];
        let recon = reconcile(&p, &records, false, Vec::new());
        assert!(recon.anomalies.is_empty(), "{:?}", recon.anomalies);
        assert_eq!(recon.accounted.len(), 1, "the superseded lane expects nothing");
        let report = build_continuation(&p, &recon, &records).unwrap();
        let second = report.inventory.lanes.iter().find(|l| l.label == "second").unwrap();
        assert_eq!(second.availability, "unavailable");
        assert!(second.reason.as_deref().unwrap().contains("artifacts changed"));
        assert_eq!(report.inventory.availability, "partial");
    }

    #[test]
    fn shell_quoting_keeps_a_command_pasteable() {
        assert_eq!(shell_quote("123-4"), "123-4");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_display(&continuation_argv("1-2")), "brokkr test --from-run 1-2");
    }

    /// The `--json` shape: the source run id, an explicit statement that it
    /// certifies nothing, the inventory's availability and completeness, every
    /// candidate's full execution identity, outcome and detail, and the
    /// command as structured argv and cwd beside its display string.
    #[test]
    fn the_report_serializes_the_documented_object() {
        let u = unit("integration");
        let p = plan_of(vec![lane(0, "default", LaneKind::Serial, &[(&u, "a"), (&u, "b")])]);
        let records = vec![obs(0, 1, &u, ObsEvent::Started { name: "a".into() }), deadline()];
        let recon = reconcile(&p, &records, false, Vec::new());
        let report = build_continuation(&p, &recon, &records).unwrap();
        let v: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(v["source_run_id"], "123456-2");
        assert_eq!(v["certifies"], false);
        assert!(v["statement"].as_str().unwrap().contains("certifies nothing"));
        assert_eq!(v["inventory"]["scope"], "binary_tests");
        assert_eq!(v["inventory"]["availability"], "complete");
        assert_eq!(v["inventory"]["plan_complete"], true);
        assert_eq!(v["inventory"]["journal_closed"], false);
        assert_eq!(v["inventory"]["unresolved"], 2);
        assert_eq!(v["inventory"]["lanes"][0]["availability"], "complete");
        let first = &v["candidates"][0];
        assert_eq!(first["outcome"], "interrupted");
        assert_eq!(first["detail"], "phase_deadline");
        assert_eq!(first["test"], "a");
        assert_eq!(first["execution"]["lane"], 0);
        assert_eq!(first["execution"]["attempt"], 1);
        assert_eq!(first["execution"]["pair"]["unit"]["target"], "integration");
        assert_eq!(v["candidates"][1]["outcome"], "unobserved");
        assert_eq!(v["replay"]["available"], true);
        assert_eq!(v["replay"]["command"]["argv"], serde_json::json!(["brokkr", "test", "--from-run", "123456-2"]));
        assert_eq!(v["replay"]["command"]["cwd"], "/x");
        assert_eq!(v["replay"]["command"]["display"], "brokkr test --from-run 123456-2");
        assert!(v["replay"]["environment"].as_str().unwrap().contains("not a snapshot"));
        assert!(v.get("reached").is_none(), "the rendering aid is not part of the trailer");
    }

    /// A binary of a partial lane the listing could not enumerate is stated,
    /// not hidden: the lane's inventory is partial, with the binary and why.
    #[test]
    fn an_unenumerable_binary_makes_its_lane_partial_and_says_so() {
        let u = unit("integration");
        let mut l = lane(0, "default", LaneKind::Serial, &[(&u, "a")]);
        l.unlisted = vec![("crate-a::custom".into(), "no libtest listing".into())];
        let p = plan_of(vec![l]);
        let records = vec![deadline()];
        let recon = reconcile(&p, &records, false, Vec::new());
        let report = build_continuation(&p, &recon, &records).unwrap();
        assert_eq!(report.inventory.availability, "partial");
        let text = render_continuation(&report).join("\n");
        assert!(text.contains("inventory partial for default"), "{text}");
        assert!(text.contains("crate-a::custom (no libtest listing)"), "{text}");
    }

    /// Fail-fast casualties are counted, not listed; an execution a kill left
    /// is still named. The full list stays in the `--json` candidates.
    #[test]
    fn fail_fast_casualties_are_counted_not_named() {
        let first = unit("first");
        let later = unit("second");
        let p = plan_of(vec![lane(
            0,
            "default",
            LaneKind::Serial,
            &[(&first, "a"), (&first, "b"), (&later, "c"), (&later, "d")],
        )]);
        let records = vec![
            obs(0, 1, &first, ObsEvent::Started { name: "a".into() }),
            obs(0, 1, &first, ObsEvent::Finished { name: "a".into(), result: TestResult::Failed }),
            JournalRecord::Terminated(Termination {
                scope: TerminationScope::Lane,
                lane: Some(0),
                stream: None,
                cause: TerminationCause::FailFast,
                test: None,
                charged: None,
            }),
        ];
        let recon = reconcile(&p, &records, false, Vec::new());
        let report = build_continuation(&p, &recon, &records).unwrap();
        assert_eq!(report.candidates.len(), 3, "--json still carries every one");
        let text = render_continuation(&report).join("\n");
        assert!(text.contains("default / crate-a / test:first\n  not reached: 1 test"), "{text}");
        assert!(text.contains("default / crate-a / test:second: not reached, 2 tests"), "{text}");
        assert!(!text.contains("  unobserved"), "{text}");
        assert!(!text.contains(" c "), "{text}");
        let full = render_continuation_full(&report).join("\n");
        assert!(full.contains("  unobserved   c  fail fast"), "{full}");
        assert!(full.contains("  unobserved   d  fail fast"), "{full}");
        assert!(full.contains(REPLAY_ENVIRONMENT), "{full}");
        assert!(full.contains("missing evidence is not proof of non-execution"), "{full}");
        assert!(!full.contains("--json trailer"), "{full}");
    }

    /// A group of which nothing was observed is "not reached"; the binary a
    /// stop actually killed is not.
    #[test]
    fn later_binaries_are_distinguished_from_the_one_killed() {
        let killed = unit("first");
        let later = unit("second");
        let p = plan_of(vec![lane(0, "default", LaneKind::Serial, &[(&killed, "a"), (&later, "b")])]);
        let records = vec![obs(0, 1, &killed, ObsEvent::Started { name: "a".into() }), deadline()];
        let recon = reconcile(&p, &records, false, Vec::new());
        let text = render_continuation(&build_continuation(&p, &recon, &records).unwrap()).join("\n");
        assert!(text.contains("default / crate-a / test:first\n"), "{text}");
        assert!(text.contains("default / crate-a / test:second (not reached)"), "{text}");
    }
}
