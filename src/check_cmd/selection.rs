// Invocation selection: which lanes each phase of `brokkr check` and
// `brokkr test` is ELIGIBLE to attempt, and with which package selection.
//
// One decision, made once, before any phase runs. Every consumer - the clippy
// and rustdoc per-shape loop, the test phase, preparation, the lane runners,
// install-feature, `brokkr test` and the announcements - reads the answer
// instead of re-deriving it from the sweep and the CLI `-p` set. Re-deriving it
// in each place is how two of them came to disagree: preparation built a
// lane's support binaries before it applied the `-p` rules, so a lane the rules
// excluded still compiled them; and `brokkr test` deduped on a key that left
// out `doc_only`, so a sweep and its doctest twin collapsed into one run.
//
// What this module does NOT do: predict binaries, tests, inventory or outcomes.
// "Eligible" is a disposition, never a promise - an earlier failure, a stop or
// an empty listing can still leave an eligible lane unrun. Prepared inventory
// (`prepare.rs`) and runtime observations (the journal, the accounting,
// `LaneTap`, `brokkr test`'s per-iteration lanes) keep their own models.
//
// Every per-phase selection is ALIGNED with the active sweeps - one entry per
// sweep, in order - because the sweep index is the lane identity everywhere
// (`LaneRecord`, prepared-lane lookup, `LaneTap`, termination labels). Nothing
// is filtered first and enumerated after.
//
// The types live in a nested module so their invariants are enforced by
// privacy, not by convention: an `Attempt`, a `Deduped` or an `Excluded` entry
// can only be built here, where the rules below hold for every one of them.

mod selection {
    use crate::config::{BinConfig, Certifies, EffectiveUnification};
    use crate::error::DevError;
    use crate::profile::ResolvedSweep;

    /// The phases a per-sweep selection exists for. Preparation reads Test's.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum SelectionPhase {
        Clippy,
        Rustdoc,
        Test,
    }

    impl SelectionPhase {
        pub(crate) fn name(self) -> &'static str {
            match self {
                Self::Clippy => "clippy",
                Self::Rustdoc => "rustdoc",
                Self::Test => "test",
            }
        }

        /// `test_exclude_packages` narrows the test invocation only: clippy
        /// and rustdoc stay workspace-wide.
        fn honours_test_exclusions(self) -> bool {
            self == Self::Test
        }
    }

    /// Where `brokkr test`'s one package came from.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum PackageSource {
        /// `-p` on the command line.
        Cli,
        /// `[test] default_package`.
        DefaultPackage,
        /// The project's built-in default package.
        ProjectDefault,
    }

    /// Who replaced a sweep's own selection.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Provenance {
        /// `brokkr check -p` / `brokkr clippy -p`.
        Cli,
        /// `brokkr test`'s resolved package, whatever its source.
        ResolvedPackage(PackageSource),
    }

    /// The effective package selection of one phase of one sweep.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Selection {
        /// The sweep's own `packages` list.
        Explicit(Vec<String>),
        /// `--workspace --exclude ...` from `test_exclude_packages`. Test phase
        /// only - the exclusions never narrow clippy or rustdoc.
        WorkspaceExcluding(Vec<String>),
        /// No package flags: cargo's default selection (the default members
        /// of a virtual workspace, the root package otherwise). Not the same
        /// claim as workspace coverage, and never described as one.
        Bare,
        /// An invocation-level package set REPLACING the sweep's own selection
        /// (cargo unions selection flags, so it can never combine with it).
        /// The packages are the ones the sweep's rules admitted.
        Override { packages: Vec<String>, provenance: Provenance },
    }

    impl Selection {
        /// The sweep's own selection for `phase`, with no invocation override.
        /// Parse refuses a sweep setting both `packages` and
        /// `test_exclude_packages`, so the order here never decides anything.
        pub(crate) fn configured(sweep: &ResolvedSweep, phase: SelectionPhase) -> Self {
            if !sweep.packages.is_empty() {
                Self::Explicit(unique(&sweep.packages))
            } else if phase.honours_test_exclusions() && !sweep.test_exclude_packages.is_empty() {
                Self::WorkspaceExcluding(unique(&sweep.test_exclude_packages))
            } else {
                Self::Bare
            }
        }

        /// The packages a positive selection names; `None` for `Bare` and
        /// `WorkspaceExcluding`, which name none.
        pub(crate) fn packages(&self) -> Option<&[String]> {
            match self {
                Self::Explicit(p) | Self::Override { packages: p, .. } => Some(p),
                Self::WorkspaceExcluding(_) | Self::Bare => None,
            }
        }

        /// The same selection narrowed to one of its packages: one cargo
        /// resolution of a per-package lane. Keeps the provenance.
        fn narrowed_to(&self, pkg: &str) -> Self {
            match self {
                Self::Override { provenance, .. } => {
                    Self::Override { packages: vec![pkg.to_owned()], provenance: *provenance }
                }
                _ => Self::Explicit(vec![pkg.to_owned()]),
            }
        }
    }

    /// The cargo package-selection flags of a selection. The ONE place they
    /// are spelled: clippy, rustdoc, the pre-builds, the test runs, the
    /// prebuilds and listings, the coverage enumeration and `brokkr test` all
    /// call this.
    pub(crate) fn package_args(selection: &Selection) -> Vec<String> {
        match selection {
            Selection::Explicit(p) | Selection::Override { packages: p, .. } => {
                p.iter().flat_map(|pkg| ["-p".to_owned(), pkg.clone()]).collect()
            }
            Selection::WorkspaceExcluding(excluded) => {
                // `--exclude` requires `--workspace`.
                let mut args = vec!["--workspace".to_owned()];
                for pkg in excluded {
                    args.push("--exclude".to_owned());
                    args.push(pkg.clone());
                }
                args
            }
            Selection::Bare => Vec::new(),
        }
    }

    /// How many cargo resolutions a lane's selection is run as.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Resolutions {
        /// One resolution over the whole selection.
        Combined,
        /// One resolution per package (`feature_unification = "package"`):
        /// a batched multi-`-p` run resolves a graph none of them install
        /// under. Always EQUAL to the selection's package list - same set,
        /// same order, no duplicates.
        PerPackage(Vec<String>),
    }

    impl Resolutions {
        /// The only constructor: derived from the selection, so it cannot
        /// disagree with it. Package mode over a selection that names no
        /// packages is a construction error, never repaired into one
        /// unscoped resolution.
        fn for_selection(
            sweep: &ResolvedSweep,
            selection: &Selection,
            phase: SelectionPhase,
        ) -> Result<Self, DevError> {
            if let Some(p) = selection.packages()
                && has_duplicates(p)
            {
                return Err(DevError::Config(format!(
                    "sweep '{}': its {} selection names a package twice ({}) - a brokkr bug, \
                     please report the `[[check]]` entry",
                    sweep.label,
                    phase.name(),
                    p.join(", ")
                )));
            }
            if !sweep.effective_unification.is_per_package() {
                return Ok(Self::Combined);
            }
            match selection.packages() {
                Some(p) => Ok(Self::PerPackage(p.to_vec())),
                None => Err(DevError::Config(format!(
                    "sweep '{}' resolves once per package (`feature_unification = \"package\"`), \
                     but its {} selection names no packages; package mode needs an explicit \
                     `packages` list",
                    sweep.label,
                    phase.name()
                ))),
            }
        }
    }

    /// One cargo resolution of an attempted lane: the selection that one cargo
    /// invocation runs with.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct ResolutionRun {
        /// `Some(pkg)` under package mode; `None` for the combined resolution.
        pub(crate) resolution: Option<String>,
        pub(crate) selection: Selection,
    }

    /// Which package rule ruled a `-p` package out of a sweep.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum AdmissionRule {
        /// The sweep's `test_exclude_packages` names it (test phase only).
        TestExcluded,
        /// The sweep has a `packages` list and it is not on it.
        NotListed,
    }

    /// A package the invocation named and a sweep's rules did not admit.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct AdmissionNote {
        pub(crate) package: String,
        pub(crate) rule: AdmissionRule,
    }

    impl std::fmt::Display for AdmissionNote {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.rule {
                AdmissionRule::TestExcluded => {
                    write!(f, "-p {} is in this sweep's test_exclude_packages", self.package)
                }
                AdmissionRule::NotListed => {
                    write!(f, "-p {} is not in this sweep's packages list", self.package)
                }
            }
        }
    }

    /// What a sweep's rules made of the invocation's package set.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub(crate) struct Admission {
        /// The admitted packages, in invocation order, each once.
        pub(crate) kept: Vec<String>,
        /// One note per package not admitted, each once.
        pub(crate) notes: Vec<AdmissionNote>,
    }

    /// The package rules. The exclusion list is checked before the packages
    /// list, so a package both rule out is reported as excluded.
    fn admit(sweep: &ResolvedSweep, packages: &[String], phase: SelectionPhase) -> Admission {
        let mut out = Admission::default();
        for pkg in packages {
            let rule = if phase.honours_test_exclusions() && sweep.test_exclude_packages.contains(pkg) {
                Some(AdmissionRule::TestExcluded)
            } else if !sweep.packages.is_empty() && !sweep.packages.contains(pkg) {
                Some(AdmissionRule::NotListed)
            } else {
                None
            };
            match rule {
                Some(rule) => {
                    let note = AdmissionNote { package: pkg.clone(), rule };
                    if !out.notes.contains(&note) {
                        out.notes.push(note);
                    }
                }
                None => {
                    if !out.kept.contains(pkg) {
                        out.kept.push(pkg.clone());
                    }
                }
            }
        }
        out
    }

    /// A lane the phase is eligible to attempt.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Attempt {
        selection: Selection,
        resolutions: Resolutions,
        notes: Vec<AdmissionNote>,
    }

    impl Attempt {
        pub(crate) fn selection(&self) -> &Selection {
            &self.selection
        }

        /// Consumers iterate [`Self::runs`]; this is the invariant's witness.
        #[cfg(test)]
        pub(crate) fn resolutions(&self) -> &Resolutions {
            &self.resolutions
        }

        /// The invocation packages this sweep's rules did not admit.
        pub(crate) fn notes(&self) -> &[AdmissionNote] {
            &self.notes
        }

        /// The cargo resolutions this lane runs as, in order.
        pub(crate) fn runs(&self) -> Vec<ResolutionRun> {
            match &self.resolutions {
                Resolutions::Combined => {
                    vec![ResolutionRun { resolution: None, selection: self.selection.clone() }]
                }
                Resolutions::PerPackage(packages) => packages
                    .iter()
                    .map(|p| ResolutionRun { resolution: Some(p.clone()), selection: self.selection.narrowed_to(p) })
                    .collect(),
            }
        }
    }

    /// A lane whose rules admit none of the invocation's packages.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Excluded {
        notes: Vec<AdmissionNote>,
    }

    impl Excluded {
        pub(crate) fn notes(&self) -> &[AdmissionNote] {
            &self.notes
        }

        /// Every note, joined: why the lane is not attempted.
        pub(crate) fn reason(&self) -> String {
            join_notes(&self.notes)
        }
    }

    /// A lane whose work an EARLIER attempted lane of the same phase already
    /// covers.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Deduped {
        onto: usize,
        selection: Selection,
        notes: Vec<AdmissionNote>,
    }

    impl Deduped {
        /// The index of the earlier attempted lane.
        pub(crate) fn onto(&self) -> usize {
            self.onto
        }

        pub(crate) fn selection(&self) -> &Selection {
            &self.selection
        }

        pub(crate) fn notes(&self) -> &[AdmissionNote] {
            &self.notes
        }
    }

    /// A lane this phase does not apply to at all (rustdoc's doctest
    /// carriers), with what the rules made of the invocation regardless.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct NotApplicable {
        reason: &'static str,
        admission: Admission,
    }

    impl NotApplicable {
        pub(crate) fn reason(&self) -> &'static str {
            self.reason
        }

        pub(crate) fn admission(&self) -> &Admission {
            &self.admission
        }
    }

    /// One sweep's disposition in one phase. An enum by disposition, so a
    /// pairing that makes no sense (a deduped lane with resolutions, an
    /// excluded lane with a selection) cannot be built.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum LaneEntry {
        /// The phase does not run: no admission was computed.
        Disabled(&'static str),
        Excluded(Excluded),
        Attempt(Attempt),
        Deduped(Deduped),
        NotApplicable(NotApplicable),
    }

    impl LaneEntry {
        pub(crate) fn attempt(&self) -> Option<&Attempt> {
            match self {
                Self::Attempt(a) => Some(a),
                _ => None,
            }
        }

        /// The invocation packages this sweep's rules did not admit, whatever
        /// its disposition.
        pub(crate) fn notes(&self) -> &[AdmissionNote] {
            match self {
                Self::Disabled(_) => &[],
                Self::Excluded(e) => e.notes(),
                Self::Attempt(a) => a.notes(),
                Self::Deduped(d) => d.notes(),
                Self::NotApplicable(n) => &n.admission().notes,
            }
        }

        /// Why this lane is not attempted; `None` for an attempt.
        pub(crate) fn skip_reason(&self) -> Option<String> {
            match self {
                Self::Attempt(_) => None,
                Self::Disabled(r) | Self::NotApplicable(NotApplicable { reason: r, .. }) => Some((*r).to_owned()),
                Self::Excluded(e) => Some(e.reason()),
                Self::Deduped(d) => Some(format!("deduped onto lane {}", d.onto)),
            }
        }
    }

    const PHASE_SKIPPED: &str = "the phase does not run";
    const DOCTEST_CARRIER: &str = "doctest carrier, no build shape of its own";

    /// What a phase dedupes on.
    enum Dedupe<'a> {
        /// `check`'s test lanes never dedupe: their filters and execution
        /// policies differ even where their builds do not.
        Never,
        /// Clippy and rustdoc: two lanes of one build shape are one lint (or
        /// doc) surface.
        BuildShape,
        /// `brokkr test`: two lanes that would execute identically
        /// ([`ExecProjection`]), with each sweep's effective debug answer.
        Execution(&'a dyn Fn(&ResolvedSweep) -> bool),
    }

    /// One phase's selection: one entry per active sweep, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct PhaseSelection {
        phase: SelectionPhase,
        enabled: bool,
        /// The invocation's override package set (CLI `-p`, or `brokkr test`'s
        /// package); empty when the sweeps' own selections stand.
        overrides: Vec<String>,
        entries: Vec<LaneEntry>,
        /// "Nothing reached this phase" - stored here at construction and
        /// reported at the phase boundary, never raised here.
        refusal: Option<String>,
    }

    impl PhaseSelection {
        /// A phase that does not run: every entry `Disabled`, nothing
        /// validated, no refusal.
        pub(crate) fn disabled(phase: SelectionPhase, sweeps: usize, reason: &'static str) -> Self {
            Self {
                phase,
                enabled: false,
                overrides: Vec::new(),
                entries: vec![LaneEntry::Disabled(reason); sweeps],
                refusal: None,
            }
        }

        /// `brokkr check`'s (and `brokkr clippy`'s) selection for one enabled
        /// phase. `cli` is the CLI `-p` set; empty leaves every sweep's own
        /// selection standing.
        pub(crate) fn for_check(
            phase: SelectionPhase,
            sweeps: &[ResolvedSweep],
            cli: &[String],
        ) -> Result<Self, DevError> {
            let dedupe = match phase {
                SelectionPhase::Test => Dedupe::Never,
                SelectionPhase::Clippy | SelectionPhase::Rustdoc => Dedupe::BuildShape,
            };
            let provenance = (!cli.is_empty()).then_some(Provenance::Cli);
            let mut selection = Self::construct(phase, sweeps, cli, provenance, &dedupe)?;
            selection.refusal = selection.nothing_reached(sweeps);
            Ok(selection)
        }

        /// `brokkr test`'s selection: its resolved package ALWAYS overrides,
        /// whatever its source, and lanes that would execute identically
        /// dedupe ([`ExecProjection`]). `debug_of` is each sweep's effective
        /// debug answer (`resolve_debug`). Apply `--sweep` before this.
        pub(crate) fn for_brokkr_test(
            sweeps: &[ResolvedSweep],
            package: &str,
            source: PackageSource,
            debug_of: &dyn Fn(&ResolvedSweep) -> bool,
        ) -> Result<Self, DevError> {
            Self::construct(
                SelectionPhase::Test,
                sweeps,
                &[package.to_owned()],
                Some(Provenance::ResolvedPackage(source)),
                &Dedupe::Execution(debug_of),
            )
        }

        fn construct(
            phase: SelectionPhase,
            sweeps: &[ResolvedSweep],
            overrides: &[String],
            provenance: Option<Provenance>,
            dedupe: &Dedupe<'_>,
        ) -> Result<Self, DevError> {
            let mut entries: Vec<LaneEntry> = Vec::with_capacity(sweeps.len());
            let mut shapes: Vec<(crate::profile::BuildShapeKey, usize)> = Vec::new();
            let mut projections: Vec<(ExecProjection, usize)> = Vec::new();
            for (i, sweep) in sweeps.iter().enumerate() {
                let admission = match provenance {
                    Some(_) => admit(sweep, overrides, phase),
                    None => Admission::default(),
                };
                if phase == SelectionPhase::Rustdoc && sweep.doc_only {
                    entries.push(LaneEntry::NotApplicable(NotApplicable { reason: DOCTEST_CARRIER, admission }));
                    continue;
                }
                let Admission { kept, notes } = admission;
                let selection = match provenance {
                    Some(_) if kept.is_empty() => {
                        entries.push(LaneEntry::Excluded(Excluded { notes }));
                        continue;
                    }
                    Some(provenance) => Selection::Override { packages: kept, provenance },
                    None => Selection::configured(sweep, phase),
                };
                // Validated for every lane that has a selection, deduped ones
                // included: an invalid pairing is never repaired, nor hidden.
                let resolutions = Resolutions::for_selection(sweep, &selection, phase)?;
                let onto = match dedupe {
                    Dedupe::Never => None,
                    Dedupe::BuildShape => {
                        let key = sweep.build_shape_key();
                        let onto = shapes.iter().find(|(k, _)| *k == key).map(|(_, j)| *j);
                        if onto.is_none() {
                            shapes.push((key, i));
                        }
                        onto
                    }
                    Dedupe::Execution(debug_of) => {
                        let p = ExecProjection::of(sweep, &selection, &resolutions, debug_of(sweep));
                        let onto = projections.iter().find(|(k, _)| *k == p).map(|(_, j)| *j);
                        if onto.is_none() {
                            projections.push((p, i));
                        }
                        onto
                    }
                };
                entries.push(match onto {
                    Some(onto) => LaneEntry::Deduped(Deduped { onto, selection, notes }),
                    None => LaneEntry::Attempt(Attempt { selection, resolutions, notes }),
                });
            }
            Ok(Self { phase, enabled: true, overrides: overrides.to_vec(), entries, refusal: None })
        }

        /// The refusal of a run whose CLI `-p` rules every applicable sweep
        /// out of this phase: nothing would be checked, which must not read as
        /// clean. A phase with no applicable sweep at all (rustdoc over only
        /// doctest carriers) is not refused.
        fn nothing_reached(&self, sweeps: &[ResolvedSweep]) -> Option<String> {
            if !self.enabled || self.overrides.is_empty() {
                return None;
            }
            if self.entries.iter().any(|e| matches!(e, LaneEntry::Attempt(_))) {
                return None;
            }
            let applicable: Vec<&str> = self
                .entries
                .iter()
                .zip(sweeps)
                .filter(|(e, _)| !matches!(e, LaneEntry::NotApplicable(_) | LaneEntry::Disabled(_)))
                .map(|(_, s)| s.label.as_str())
                .collect();
            if applicable.is_empty() {
                return None;
            }
            let scope = self.overrides.join(" -p ");
            Some(match self.phase {
                SelectionPhase::Test => {
                    format!("-p {scope}: every sweep's config rules the selection out; zero tests ran")
                }
                SelectionPhase::Clippy | SelectionPhase::Rustdoc => format!(
                    "-p {scope}: every sweep's config rules the selection out ({}); nothing reached {}",
                    applicable.join(", "),
                    self.phase.name()
                ),
            })
        }

        pub(crate) fn phase(&self) -> SelectionPhase {
            self.phase
        }

        pub(crate) fn is_enabled(&self) -> bool {
            self.enabled
        }

        /// The invocation's override package set; empty when none.
        pub(crate) fn overrides(&self) -> &[String] {
            &self.overrides
        }

        pub(crate) fn entries(&self) -> &[LaneEntry] {
            &self.entries
        }

        pub(crate) fn entry(&self, sweep: usize) -> Option<&LaneEntry> {
            self.entries.get(sweep)
        }

        pub(crate) fn attempt(&self, sweep: usize) -> Option<&Attempt> {
            self.entry(sweep).and_then(LaneEntry::attempt)
        }

        pub(crate) fn refusal(&self) -> Option<&str> {
            self.refusal.as_deref()
        }

        /// Report the stored refusal, at the phase boundary.
        pub(crate) fn check_refusal(&self) -> Result<(), DevError> {
            match &self.refusal {
                Some(m) => Err(DevError::Config(m.clone())),
                None => Ok(()),
            }
        }
    }

    /// What `brokkr test` dedupes on: everything that decides what a lane
    /// builds and executes, and nothing about where its selection came from.
    ///
    /// The effective package selection WITHOUT provenance, the resolutions,
    /// features, unification, rustflags, env, `build_packages`, the effective
    /// debug answer (replacing the configured profile, which `--debug`/
    /// `--release` can override), and `doc_only` - a sweep and its doctest
    /// twin run different things. Admission notes are outside it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct ExecProjection {
        packages: ProjectedSelection,
        resolutions: Resolutions,
        features: Vec<String>,
        unification: EffectiveUnification,
        rustflags: Vec<String>,
        env: Vec<(String, String)>,
        build_packages: Vec<String>,
        debug: bool,
        doc_only: bool,
    }

    /// A selection with its provenance dropped.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum ProjectedSelection {
        Packages(Vec<String>),
        WorkspaceExcluding(Vec<String>),
        Bare,
    }

    impl ExecProjection {
        pub(crate) fn of(
            sweep: &ResolvedSweep,
            selection: &Selection,
            resolutions: &Resolutions,
            debug: bool,
        ) -> Self {
            let packages = match selection {
                Selection::Explicit(p) | Selection::Override { packages: p, .. } => {
                    ProjectedSelection::Packages(p.clone())
                }
                Selection::WorkspaceExcluding(x) => ProjectedSelection::WorkspaceExcluding(x.clone()),
                Selection::Bare => ProjectedSelection::Bare,
            };
            Self {
                packages,
                resolutions: resolutions.clone(),
                features: sweep.cargo_feature_args.clone(),
                unification: sweep.effective_unification,
                rustflags: sweep.rustflags.clone(),
                env: sweep.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                build_packages: sweep.build_packages.clone(),
                debug,
                doc_only: sweep.doc_only,
            }
        }
    }

    /// The install-feature phase's selection.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum InstallSelection {
        /// The phase does not apply to this run, silently: no `[bin]`, an
        /// empty install set, the mode rules the claim out, or the phase is
        /// skipped.
        NotApplicable(&'static str),
        /// The CLI `-p` set rules every install package out. A visible skip,
        /// never a refusal.
        Skipped(&'static str),
        /// The install packages to check, in config order. Which of their bins
        /// are eligible stays a runtime fact (cargo's own answer).
        Eligible { packages: Vec<String>, configured: usize, debug: bool },
    }

    impl InstallSelection {
        pub(crate) fn build(
            bin_cfg: Option<&BinConfig>,
            cli: &[String],
            certifies: Option<Certifies>,
            enabled: bool,
        ) -> Self {
            if !enabled {
                return Self::NotApplicable(PHASE_SKIPPED);
            }
            let Some(cfg) = bin_cfg else {
                return Self::NotApplicable("no [bin] table");
            };
            if cfg.install.is_empty() {
                return Self::NotApplicable("no [bin] install packages");
            }
            if !super::install_feature_applies(cfg.install_feature_check, certifies) {
                return Self::NotApplicable("install_feature_check rules this run out");
            }
            // CLI `-p` intersects, matching every other phase. (Under the gate
            // this is unreachable - a complete profile refuses `-p` outright.)
            let packages: Vec<String> = if cli.is_empty() {
                cfg.install.clone()
            } else {
                cfg.install.iter().filter(|p| cli.contains(p)).cloned().collect()
            };
            if packages.is_empty() {
                return Self::Skipped("-p rules the install set out");
            }
            Self::Eligible { packages, configured: cfg.install.len(), debug: cfg.debug }
        }
    }

    /// Every selection one `brokkr check` run makes.
    #[derive(Debug, Clone)]
    pub(crate) struct CheckSelections {
        pub(crate) clippy: PhaseSelection,
        pub(crate) rustdoc: PhaseSelection,
        pub(crate) test: PhaseSelection,
        pub(crate) install: InstallSelection,
    }

    impl CheckSelections {
        /// `skip` is the run's phase-skip predicate (profile `skip_phases` and
        /// the prose-only shortcut); a skipped phase constructs nothing.
        pub(crate) fn build(
            sweeps: &[ResolvedSweep],
            cli: &[String],
            skip: &dyn Fn(&str) -> bool,
            rustdoc_configured: bool,
            bin_cfg: Option<&BinConfig>,
            certifies: Option<Certifies>,
        ) -> Result<Self, DevError> {
            let phase = |p: SelectionPhase, off: Option<&'static str>| match off {
                Some(reason) => Ok(PhaseSelection::disabled(p, sweeps.len(), reason)),
                None => PhaseSelection::for_check(p, sweeps, cli),
            };
            let rustdoc_off = if skip("rustdoc") {
                Some(PHASE_SKIPPED)
            } else if !rustdoc_configured {
                Some("no [rustdoc] table")
            } else {
                None
            };
            Ok(Self {
                clippy: phase(SelectionPhase::Clippy, skip("clippy").then_some(PHASE_SKIPPED))?,
                rustdoc: phase(SelectionPhase::Rustdoc, rustdoc_off)?,
                test: phase(SelectionPhase::Test, skip("test").then_some(PHASE_SKIPPED))?,
                install: InstallSelection::build(bin_cfg, cli, certifies, !skip("install_feature")),
            })
        }
    }

    /// Which sweeps a run actually worked on, for the `--json` trailer's
    /// `sweeps` list: one flag per sweep for the diagnostics group (clippy and
    /// rustdoc share it) and one for the test phase, each set when the phase
    /// starts working on that sweep - before cargo returns, so a lane that
    /// failed still counts as reached. The trailer lists their union: a sweep
    /// may be reached by one group, the other, or both.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub(crate) struct ReachLedger {
        diagnostics: Vec<bool>,
        test: Vec<bool>,
    }

    impl ReachLedger {
        pub(crate) fn new(sweeps: usize) -> Self {
            Self { diagnostics: vec![false; sweeps], test: vec![false; sweeps] }
        }

        pub(crate) fn reach_diagnostics(&mut self, sweep: usize) {
            if let Some(slot) = self.diagnostics.get_mut(sweep) {
                *slot = true;
            }
        }

        pub(crate) fn reach_test(&mut self, sweep: usize) {
            if let Some(slot) = self.test.get_mut(sweep) {
                *slot = true;
            }
        }

        /// The labels of the sweeps either group reached, in sweep order.
        pub(crate) fn reached_labels<'a>(&self, sweeps: &'a [ResolvedSweep]) -> Vec<&'a str> {
            sweeps
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    self.diagnostics.get(*i).copied().unwrap_or(false) || self.test.get(*i).copied().unwrap_or(false)
                })
                .map(|(_, s)| s.label.as_str())
                .collect()
        }
    }

    pub(crate) fn join_notes(notes: &[AdmissionNote]) -> String {
        notes.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
    }

    fn unique(list: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(list.len());
        for item in list {
            if !out.contains(item) {
                out.push(item.clone());
            }
        }
        out
    }

    fn has_duplicates(list: &[String]) -> bool {
        list.iter().enumerate().any(|(i, a)| list[..i].contains(a))
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use super::*;
        use crate::config::{CargoUnification, SweepProfile};

        fn s(list: &[&str]) -> Vec<String> {
            list.iter().map(|p| (*p).to_owned()).collect()
        }

        fn sweep(label: &str) -> ResolvedSweep {
            ResolvedSweep { label: label.into(), ..ResolvedSweep::default() }
        }

        fn listed(label: &str, packages: &[&str]) -> ResolvedSweep {
            ResolvedSweep { packages: s(packages), ..sweep(label) }
        }

        fn excluding(label: &str, excluded: &[&str]) -> ResolvedSweep {
            ResolvedSweep { test_exclude_packages: s(excluded), ..sweep(label) }
        }

        fn package_mode(mut sweep: ResolvedSweep) -> ResolvedSweep {
            sweep.effective_unification = EffectiveUnification::Pinned(CargoUnification::Package);
            sweep
        }

        fn cli(packages: &[&str]) -> Selection {
            Selection::Override { packages: s(packages), provenance: Provenance::Cli }
        }

        #[test]
        fn the_exclusion_rule_is_checked_before_the_packages_list() {
            // A package both rules rule out is reported as excluded.
            let both = ResolvedSweep { packages: s(&["a"]), test_exclude_packages: s(&["x"]), ..sweep("t") };
            let a = admit(&both, &s(&["x"]), SelectionPhase::Test);
            assert_eq!(a.notes, vec![AdmissionNote { package: "x".into(), rule: AdmissionRule::TestExcluded }]);
            // Outside the test phase the exclusion list does not exist.
            let a = admit(&both, &s(&["x"]), SelectionPhase::Clippy);
            assert_eq!(a.notes[0].rule, AdmissionRule::NotListed);
            assert_eq!(
                a.notes[0].to_string(),
                "-p x is not in this sweep's packages list"
            );
        }

        #[test]
        fn admission_keeps_invocation_order_each_package_once() {
            let a = admit(&sweep("w"), &s(&["b", "a", "b"]), SelectionPhase::Test);
            assert_eq!(a.kept, s(&["b", "a"]));
            assert!(a.notes.is_empty());
        }

        #[test]
        fn test_exclusions_narrow_the_test_phase_only() {
            let sweeps = vec![excluding("default", &["x"])];
            let clippy = PhaseSelection::for_check(SelectionPhase::Clippy, &sweeps, &[]).unwrap();
            assert_eq!(clippy.attempt(0).unwrap().selection(), &Selection::Bare);
            let test = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
            assert_eq!(test.attempt(0).unwrap().selection(), &Selection::WorkspaceExcluding(s(&["x"])));
            assert_eq!(package_args(test.attempt(0).unwrap().selection()), s(&["--workspace", "--exclude", "x"]));

            // Under `-p x` the exclusion rules the lane out of the test phase
            // and nowhere else.
            let x = s(&["x"]);
            let clippy = PhaseSelection::for_check(SelectionPhase::Clippy, &sweeps, &x).unwrap();
            assert_eq!(clippy.attempt(0).unwrap().selection(), &cli(&["x"]));
            let test = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &x).unwrap();
            assert!(matches!(test.entry(0), Some(LaneEntry::Excluded(_))));
            assert_eq!(
                test.entry(0).unwrap().skip_reason().unwrap(),
                "-p x is in this sweep's test_exclude_packages"
            );
        }

        #[test]
        fn an_override_replaces_the_sweeps_own_selection_with_what_its_rules_kept() {
            let sweeps = vec![listed("ffi", &["a", "b"])];
            let sel = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &s(&["a", "x"])).unwrap();
            let attempt = sel.attempt(0).unwrap();
            assert_eq!(attempt.selection(), &cli(&["a"]));
            assert_eq!(attempt.notes().len(), 1);
            assert_eq!(package_args(attempt.selection()), s(&["-p", "a"]));
            // Without an override the sweep's own list stands.
            let own = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
            assert_eq!(own.attempt(0).unwrap().selection(), &Selection::Explicit(s(&["a", "b"])));
        }

        #[test]
        fn package_mode_over_a_selection_naming_no_packages_is_an_error() {
            let bare = package_mode(sweep("pkg"));
            let err = PhaseSelection::for_check(SelectionPhase::Clippy, &[bare], &[]).unwrap_err().to_string();
            assert!(err.contains("names no packages"), "{err}");
            let excl = package_mode(excluding("pkg", &["x"]));
            assert!(PhaseSelection::for_check(SelectionPhase::Test, &[excl], &[]).is_err());
        }

        #[test]
        fn per_package_resolutions_equal_the_selection() {
            let sweeps = vec![package_mode(listed("pkg", &["a", "b", "c"]))];
            let own = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
            let attempt = own.attempt(0).unwrap();
            assert_eq!(attempt.resolutions(), &Resolutions::PerPackage(s(&["a", "b", "c"])));
            let runs = attempt.runs();
            assert_eq!(runs.len(), 3);
            assert_eq!(runs[1], ResolutionRun { resolution: Some("b".into()), selection: Selection::Explicit(s(&["b"])) });

            // Narrowed by `-p`, in invocation order, each once.
            let narrowed = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &s(&["c", "a", "c"])).unwrap();
            let attempt = narrowed.attempt(0).unwrap();
            assert_eq!(attempt.resolutions(), &Resolutions::PerPackage(s(&["c", "a"])));
            assert_eq!(attempt.selection().packages().unwrap(), s(&["c", "a"]).as_slice());
            assert_eq!(attempt.runs()[0].selection, cli(&["c"]));

            // Any other mode is one combined resolution over the selection.
            let combined = PhaseSelection::for_check(SelectionPhase::Test, &[listed("x", &["a", "b"])], &[]).unwrap();
            let runs = combined.attempt(0).unwrap().runs();
            assert_eq!(runs, vec![ResolutionRun { resolution: None, selection: Selection::Explicit(s(&["a", "b"])) }]);
        }

        #[test]
        fn diagnostics_dedupe_only_onto_an_earlier_attempt() {
            // Two lanes of one entry: one build shape.
            let sweeps = vec![listed("tier1/x", &["a"]), listed("tier2/x", &["a"]), listed("other", &["b"])];
            let sel = PhaseSelection::for_check(SelectionPhase::Clippy, &sweeps, &[]).unwrap();
            assert!(sel.attempt(0).is_some());
            match sel.entry(1).unwrap() {
                LaneEntry::Deduped(d) => {
                    assert_eq!(d.onto(), 0);
                    assert!(sel.attempt(d.onto()).is_some(), "onto names an attempt");
                }
                other => panic!("expected deduped, got {other:?}"),
            }
            assert!(sel.attempt(2).is_some());

            // A lane the rules exclude is never a dedupe target: the first
            // lane of the shape that is ATTEMPTED is.
            let sweeps = vec![listed("a-only", &["a"]), listed("a-only-twin", &["a"])];
            let sel = PhaseSelection::for_check(SelectionPhase::Clippy, &sweeps, &s(&["z"])).unwrap();
            assert!(sel.entries().iter().all(|e| matches!(e, LaneEntry::Excluded(_))));

            // The test phase never dedupes.
            let sweeps = vec![listed("tier1/x", &["a"]), listed("tier2/x", &["a"])];
            let test = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
            assert!(test.entries().iter().all(|e| e.attempt().is_some()));
        }

        #[test]
        fn rustdoc_does_not_apply_to_doctest_carriers() {
            let doc = ResolvedSweep { doc_only: true, ..excluding("docs", &["bin"]) };
            let sweeps = vec![sweep("default"), doc];
            let sel = PhaseSelection::for_check(SelectionPhase::Rustdoc, &sweeps, &s(&["lib"])).unwrap();
            assert!(sel.attempt(0).is_some());
            match sel.entry(1).unwrap() {
                LaneEntry::NotApplicable(n) => assert_eq!(n.admission().kept, s(&["lib"])),
                other => panic!("expected not applicable, got {other:?}"),
            }
            // A rustdoc phase over carriers alone is not refused.
            let only_doc = vec![ResolvedSweep { doc_only: true, ..sweep("docs") }];
            let sel = PhaseSelection::for_check(SelectionPhase::Rustdoc, &only_doc, &s(&["x"])).unwrap();
            assert!(sel.refusal().is_none());
        }

        #[test]
        fn a_disabled_phase_constructs_and_validates_nothing() {
            // Package mode with no packages would be a construction error -
            // but not in a phase that does not run.
            let invalid = package_mode(sweep("pkg"));
            let sweeps = [invalid.clone(), sweep("other")];
            let sel = PhaseSelection::disabled(SelectionPhase::Test, sweeps.len(), "skipped");
            assert_eq!(sel.entries().len(), 2);
            assert!(sel.entries().iter().all(|e| matches!(e, LaneEntry::Disabled(_)) && e.notes().is_empty()));
            assert!(!sel.is_enabled() && sel.refusal().is_none());

            let skip = |p: &str| p == "test" || p == "clippy";
            let all = CheckSelections::build(std::slice::from_ref(&invalid), &[], &skip, false, None, None).unwrap();
            assert!(!all.clippy.is_enabled() && !all.test.is_enabled() && !all.rustdoc.is_enabled());
            // The same sweep in an enabled phase is the construction error.
            assert!(CheckSelections::build(&[invalid], &[], &|_| false, false, None, None).is_err());
        }

        #[test]
        fn a_refusal_is_stored_not_raised() {
            let sweeps = vec![listed("ffi", &["a"]), listed("vm", &["b"])];
            let test = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &s(&["z"])).unwrap();
            assert_eq!(
                test.refusal(),
                Some("-p z: every sweep's config rules the selection out; zero tests ran")
            );
            assert!(test.check_refusal().is_err());
            let clippy = PhaseSelection::for_check(SelectionPhase::Clippy, &sweeps, &s(&["z"])).unwrap();
            assert_eq!(
                clippy.refusal(),
                Some("-p z: every sweep's config rules the selection out (ffi, vm); nothing reached clippy")
            );
            // One admitting sweep is enough; no `-p` never refuses.
            let ok = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &s(&["a"])).unwrap();
            assert!(ok.refusal().is_none());
            assert!(PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &[]).unwrap().refusal().is_none());
        }

        fn debug_of(debug: bool) -> impl Fn(&ResolvedSweep) -> bool {
            move |_| debug
        }

        #[test]
        fn exec_projection_collapses_profile_pins_under_one_override() {
            // `--debug` overrides both sweeps' pins: they execute identically.
            let dev = ResolvedSweep { profile: Some(SweepProfile::Dev), ..sweep("dev") };
            let rel = ResolvedSweep { profile: Some(SweepProfile::Release), ..sweep("rel") };
            let sel =
                PhaseSelection::for_brokkr_test(&[dev.clone(), rel.clone()], "a", PackageSource::Cli, &debug_of(true))
                    .unwrap();
            assert!(matches!(sel.entry(1), Some(LaneEntry::Deduped(d)) if d.onto() == 0));
            // Without the override each pin decides its own debug answer.
            let pinned = |s: &ResolvedSweep| s.profile == Some(SweepProfile::Dev);
            let sel = PhaseSelection::for_brokkr_test(&[dev, rel], "a", PackageSource::Cli, &pinned).unwrap();
            assert!(sel.entries().iter().all(|e| e.attempt().is_some()));
        }

        #[test]
        fn exec_projection_collapses_two_sweeps_narrowed_to_one_package() {
            let sweeps = vec![listed("ab", &["a", "b"]), listed("ac", &["a", "c"])];
            let sel = PhaseSelection::for_brokkr_test(&sweeps, "a", PackageSource::DefaultPackage, &debug_of(false))
                .unwrap();
            assert_eq!(
                sel.attempt(0).unwrap().selection(),
                &Selection::Override {
                    packages: s(&["a"]),
                    provenance: Provenance::ResolvedPackage(PackageSource::DefaultPackage)
                }
            );
            assert!(matches!(sel.entry(1), Some(LaneEntry::Deduped(_))));
        }

        #[test]
        fn exec_projection_keeps_a_doctest_twin_and_differing_support_builds_apart() {
            let twin = ResolvedSweep { doc_only: true, ..sweep("docs") };
            let sel =
                PhaseSelection::for_brokkr_test(&[sweep("default"), twin], "a", PackageSource::Cli, &debug_of(false))
                    .unwrap();
            assert!(sel.entries().iter().all(|e| e.attempt().is_some()), "the doc twin is its own run");

            let server = ResolvedSweep { build_packages: s(&["server"]), ..sweep("server") };
            let sel =
                PhaseSelection::for_brokkr_test(&[sweep("plain"), server], "a", PackageSource::Cli, &debug_of(false))
                    .unwrap();
            assert!(sel.entries().iter().all(|e| e.attempt().is_some()));
        }

        #[test]
        fn exec_projection_ignores_provenance() {
            let sw = sweep("x");
            let res = Resolutions::Combined;
            let from_cli = ExecProjection::of(&sw, &cli(&["a"]), &res, false);
            let from_default = ExecProjection::of(
                &sw,
                &Selection::Override { packages: s(&["a"]), provenance: Provenance::ResolvedPackage(PackageSource::ProjectDefault) },
                &res,
                false,
            );
            let explicit = ExecProjection::of(&sw, &Selection::Explicit(s(&["a"])), &res, false);
            assert_eq!(from_cli, from_default);
            assert_eq!(from_cli, explicit);
        }

        #[test]
        fn brokkr_test_never_dedupes_an_excluded_lane() {
            let sweeps = vec![excluding("excl", &["a"]), sweep("plain")];
            let sel = PhaseSelection::for_brokkr_test(&sweeps, "a", PackageSource::Cli, &debug_of(false)).unwrap();
            assert!(matches!(sel.entry(0), Some(LaneEntry::Excluded(_))));
            assert!(sel.attempt(1).is_some());
        }

        #[test]
        fn the_install_selection_intersects_and_skips_rather_than_refusing() {
            let cfg = BinConfig {
                install: s(&["daemon", "cli"]),
                install_feature_check: Some(crate::config::InstallFeatureCheck::Always),
                ..BinConfig::default()
            };
            assert_eq!(
                InstallSelection::build(Some(&cfg), &[], None, true),
                InstallSelection::Eligible { packages: s(&["daemon", "cli"]), configured: 2, debug: false }
            );
            assert_eq!(
                InstallSelection::build(Some(&cfg), &s(&["cli"]), None, true),
                InstallSelection::Eligible { packages: s(&["cli"]), configured: 2, debug: false }
            );
            assert!(matches!(InstallSelection::build(Some(&cfg), &s(&["x"]), None, true), InstallSelection::Skipped(_)));
            assert!(matches!(InstallSelection::build(Some(&cfg), &[], None, false), InstallSelection::NotApplicable(_)));
            assert!(matches!(InstallSelection::build(None, &[], None, true), InstallSelection::NotApplicable(_)));
            let gate = BinConfig { install_feature_check: None, ..cfg };
            assert!(matches!(InstallSelection::build(Some(&gate), &[], None, true), InstallSelection::NotApplicable(_)));
        }

        #[test]
        fn the_ledger_lists_the_union_of_both_groups_in_sweep_order() {
            let sweeps = vec![sweep("a"), sweep("b"), sweep("c")];
            let mut ledger = ReachLedger::new(3);
            assert!(ledger.reached_labels(&sweeps).is_empty());
            ledger.reach_diagnostics(0);
            ledger.reach_test(1);
            assert_eq!(ledger.reached_labels(&sweeps), vec!["a", "b"]);
            ledger.reach_test(0);
            ledger.reach_diagnostics(2);
            assert_eq!(ledger.reached_labels(&sweeps), vec!["a", "b", "c"]);
        }
    }
}

pub(crate) use selection::{
    join_notes, package_args, AdmissionNote, AdmissionRule, Attempt, CheckSelections, InstallSelection, LaneEntry,
    PackageSource, PhaseSelection, ReachLedger, ResolutionRun, Selection, SelectionPhase,
};
