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
//
// Each attempted lane also carries its cargo REQUESTS, constructed here and
// nowhere else: one `ResolutionRun` per cargo resolution, one `SupportRequest`
// per `build_packages` pre-build, each holding its package selection AND the
// feature argv projected onto it (`features.rs`). A sweep's `features` are
// written against its own selection; a run that selects fewer packages (a `-p`
// narrowing, a package-mode resolution, a support build) carries only the
// tokens its packages route. `ResolvedSweep` has no executable feature argv,
// so no cargo argv builder can bypass the projection.

mod selection {
    use std::collections::BTreeSet;

    use super::features::{project, DroppedToken, FeatureOracle, Projected, WorkspaceFeatures};
    use crate::config::{BinConfig, Certifies, EffectiveUnification, SweepProfile};
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

        /// The concrete workspace members this selection selects, as cargo
        /// run at the project root resolves it.
        fn concrete(&self, ws: &WorkspaceFeatures) -> Vec<String> {
            match self {
                Self::Explicit(p) | Self::Override { packages: p, .. } => p.clone(),
                Self::WorkspaceExcluding(excluded) => {
                    ws.member_names().into_iter().filter(|m| !excluded.contains(m)).collect()
                }
                Self::Bare => ws.default_members().to_vec(),
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

    /// One cargo resolution of an attempted lane: the request one cargo
    /// invocation runs with - its package selection and the sweep's features
    /// projected onto it. Only this module builds one (the private fields),
    /// so every argv carrying a sweep's features carries a projection.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct ResolutionRun {
        /// `Some(pkg)` under package mode; `None` for the combined resolution.
        pub(crate) resolution: Option<String>,
        pub(crate) selection: Selection,
        /// The feature argv (`--all-features`, `--no-default-features`,
        /// `--features a,b`) this run hands cargo.
        features: Vec<String>,
        /// The configured tokens this run does not carry, with why.
        dropped: Vec<DroppedToken>,
    }

    impl ResolutionRun {
        /// The package-selection flags ([`package_args`]).
        pub(crate) fn package_args(&self) -> Vec<String> {
            package_args(&self.selection)
        }

        /// The projected feature flags.
        pub(crate) fn feature_args(&self) -> &[String] {
            &self.features
        }

        /// Package then feature flags: the request's whole cargo fragment.
        pub(crate) fn cargo_args(&self) -> Vec<String> {
            let mut args = self.package_args();
            args.extend(self.features.iter().cloned());
            args
        }

        #[cfg(test)]
        pub(crate) fn dropped(&self) -> &[DroppedToken] {
            &self.dropped
        }
    }

    /// One `build_packages` pre-build: the support package alone, with the
    /// sweep's features projected onto it. The domain the tokens were written
    /// against is the phase's configured selection plus every support package.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct SupportRequest {
        package: String,
        features: Vec<String>,
        dropped: Vec<DroppedToken>,
    }

    impl SupportRequest {
        pub(crate) fn package(&self) -> &str {
            &self.package
        }

        /// `-p <package>`: its own explicit single-package selection, never
        /// the lane's test selection.
        pub(crate) fn package_args(&self) -> Vec<String> {
            package_args(&Selection::Explicit(vec![self.package.clone()]))
        }

        pub(crate) fn feature_args(&self) -> &[String] {
            &self.features
        }

        #[cfg(test)]
        pub(crate) fn dropped(&self) -> &[DroppedToken] {
            &self.dropped
        }
    }

    /// A selection with its provenance dropped: what decides what cargo
    /// selects, and nothing about who asked.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum ProjectedSelection {
        Packages(Vec<String>),
        WorkspaceExcluding(Vec<String>),
        Bare,
    }

    impl ProjectedSelection {
        fn of(selection: &Selection) -> Self {
            match selection {
                Selection::Explicit(p) | Selection::Override { packages: p, .. } => Self::Packages(p.clone()),
                Selection::WorkspaceExcluding(x) => Self::WorkspaceExcluding(x.clone()),
                Selection::Bare => Self::Bare,
            }
        }
    }

    /// The sweep's features projected onto `selected`, with `domain` (plus
    /// `extra_domain`) the selection the tokens were written against.
    ///
    /// The identity, with no metadata consulted, whenever nothing could drop:
    /// no tokens, invocation-explicit features (`brokkr check --features`
    /// names exactly what the user asked cargo for), or a selection that is
    /// the domain itself. Otherwise both sides are resolved to concrete
    /// members through the invocation's one metadata snapshot.
    fn projected_features(
        sweep: &ResolvedSweep,
        selected: &Selection,
        domain: &Selection,
        extra_domain: &[String],
        oracle: &FeatureOracle<'_>,
    ) -> Result<(Vec<String>, Vec<DroppedToken>), DevError> {
        let cfg = &sweep.features;
        let identity = (cfg.argv_with(&cfg.tokens), Vec::new());
        if cfg.tokens.is_empty() || cfg.invocation_explicit {
            return Ok(identity);
        }
        if extra_domain.iter().all(|x| selected.packages().is_some_and(|p| p.contains(x)))
            && ProjectedSelection::of(selected) == ProjectedSelection::of(domain)
        {
            return Ok(identity);
        }
        let named = |sel: &Selection| sel.packages().map(|p| p.iter().cloned().collect::<BTreeSet<String>>());
        if let (Some(p), Some(mut s)) = (named(selected), named(domain)) {
            s.extend(extra_domain.iter().cloned());
            if p == s {
                return Ok(identity);
            }
        }
        let ws = oracle.get()?;
        let p = selected.concrete(ws);
        let mut s = domain.concrete(ws);
        for x in extra_domain {
            if !s.contains(x) {
                s.push(x.clone());
            }
        }
        let Projected { kept, dropped } = project(&cfg.tokens, &p, &s, ws);
        Ok((cfg.argv_with(&kept), dropped))
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
        /// One request per resolution, in order - constructed once, with the
        /// selection, never recreated on demand.
        runs: Vec<ResolutionRun>,
        /// One request per `build_packages` entry, in order.
        support: Vec<SupportRequest>,
        /// The feature argv the lane carries in at least one run: what a
        /// lane-level description renders.
        features: Vec<String>,
    }

    impl Attempt {
        /// Build an attempt and its requests. `domain` is the sweep's
        /// configured selection for the phase (before any override or
        /// per-package split): what its feature tokens were written against.
        fn build(
            sweep: &ResolvedSweep,
            selection: Selection,
            resolutions: Resolutions,
            notes: Vec<AdmissionNote>,
            domain: &Selection,
            oracle: &FeatureOracle<'_>,
        ) -> Result<Self, DevError> {
            let selections: Vec<(Option<String>, Selection)> = match &resolutions {
                Resolutions::Combined => vec![(None, selection.clone())],
                Resolutions::PerPackage(packages) => {
                    packages.iter().map(|p| (Some(p.clone()), selection.narrowed_to(p))).collect()
                }
            };
            let mut runs = Vec::with_capacity(selections.len());
            for (resolution, sel) in selections {
                let (features, dropped) = projected_features(sweep, &sel, domain, &[], oracle)?;
                runs.push(ResolutionRun { resolution, selection: sel, features, dropped });
            }
            let mut support = Vec::with_capacity(sweep.build_packages.len());
            for pkg in &sweep.build_packages {
                let own = Selection::Explicit(vec![pkg.clone()]);
                let (features, dropped) =
                    projected_features(sweep, &own, domain, &sweep.build_packages, oracle)?;
                support.push(SupportRequest { package: pkg.clone(), features, dropped });
            }
            let cfg = &sweep.features;
            let carried: Vec<String> = cfg
                .tokens
                .iter()
                .filter(|t| runs.iter().any(|r| !r.dropped.iter().any(|d| &d.token == *t)))
                .cloned()
                .collect();
            let features = cfg.argv_with(&carried);
            Ok(Self { selection, resolutions, notes, runs, support, features })
        }

        pub(crate) fn selection(&self) -> &Selection {
            &self.selection
        }

        /// The support pre-builds, in `build_packages` order.
        pub(crate) fn support(&self) -> &[SupportRequest] {
            &self.support
        }

        /// The one request of a lane that runs a single combined resolution;
        /// `None` under package mode.
        pub(crate) fn combined_run(&self) -> Option<&ResolutionRun> {
            match (&self.resolutions, self.runs.as_slice()) {
                (Resolutions::Combined, [one]) => Some(one),
                _ => None,
            }
        }

        /// The feature argv the lane carries in at least one of its runs -
        /// for a lane-level description, never a cargo argv.
        pub(crate) fn described_features(&self) -> &[String] {
            &self.features
        }

        /// The effective key of what this lane compiles: everything
        /// compile-affecting about the sweep plus every request, provenance
        /// excluded. `profile` is `None` where the caller substitutes its own
        /// answer (`brokkr test`'s effective debug).
        fn effective_key(&self, sweep: &ResolvedSweep, profile: Option<SweepProfile>) -> EffectiveKey {
            EffectiveKey {
                profile,
                rustflags: sweep.rustflags.clone(),
                env: sweep.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                unification: sweep.effective_unification,
                runs: self
                    .runs
                    .iter()
                    .map(|r| (ProjectedSelection::of(&r.selection), r.resolution.clone(), r.features.clone()))
                    .collect(),
                support: self.support.iter().map(|s| (s.package.clone(), s.features.clone())).collect(),
            }
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
        pub(crate) fn runs(&self) -> &[ResolutionRun] {
            &self.runs
        }

        /// The request of the resolution `resolution` names.
        pub(crate) fn run_for(&self, resolution: Option<&str>) -> Option<&ResolutionRun> {
            self.runs.iter().find(|r| r.resolution.as_deref() == resolution)
        }
    }

    /// What two lanes compile, compared for dedupe: the compile-affecting
    /// parts of the build shape other than packages and features (profile,
    /// rustflags, env, unification), plus the full ordered list of run
    /// requests - effective selection without provenance, resolution
    /// boundary, projected features - and the support requests with theirs.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct EffectiveKey {
        profile: Option<SweepProfile>,
        rustflags: Vec<String>,
        env: Vec<(String, String)>,
        unification: EffectiveUnification,
        runs: Vec<(ProjectedSelection, Option<String>, Vec<String>)>,
        support: Vec<(String, Vec<String>)>,
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
        /// The tokens this lane's own requests dropped, kept although the
        /// requests themselves are not: a lane folded onto another still
        /// narrowed its own features, and that is announced like any other.
        drops: Vec<(String, DroppedToken)>,
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
        /// Clippy and rustdoc: two lanes compiling the same requests under
        /// the same compile inputs ([`EffectiveKey`]) are one lint (or doc)
        /// surface.
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
            oracle: &FeatureOracle<'_>,
        ) -> Result<Self, DevError> {
            let dedupe = match phase {
                SelectionPhase::Test => Dedupe::Never,
                SelectionPhase::Clippy | SelectionPhase::Rustdoc => Dedupe::BuildShape,
            };
            let provenance = (!cli.is_empty()).then_some(Provenance::Cli);
            let mut selection = Self::construct(phase, sweeps, cli, provenance, &dedupe, oracle)?;
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
            oracle: &FeatureOracle<'_>,
        ) -> Result<Self, DevError> {
            Self::construct(
                SelectionPhase::Test,
                sweeps,
                &[package.to_owned()],
                Some(Provenance::ResolvedPackage(source)),
                &Dedupe::Execution(debug_of),
                oracle,
            )
        }

        fn construct(
            phase: SelectionPhase,
            sweeps: &[ResolvedSweep],
            overrides: &[String],
            provenance: Option<Provenance>,
            dedupe: &Dedupe<'_>,
            oracle: &FeatureOracle<'_>,
        ) -> Result<Self, DevError> {
            let mut entries: Vec<LaneEntry> = Vec::with_capacity(sweeps.len());
            let mut shapes: Vec<(EffectiveKey, usize)> = Vec::new();
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
                // The domain the sweep's feature tokens were written against:
                // its own selection for this phase, before any override or
                // per-package split.
                let configured = Selection::configured(sweep, phase);
                let selection = match provenance {
                    Some(_) if kept.is_empty() => {
                        entries.push(LaneEntry::Excluded(Excluded { notes }));
                        continue;
                    }
                    Some(provenance) => Selection::Override { packages: kept, provenance },
                    None => configured.clone(),
                };
                // Validated for every lane that has a selection, deduped ones
                // included: an invalid pairing is never repaired, nor hidden.
                let resolutions = Resolutions::for_selection(sweep, &selection, phase)?;
                let attempt = Attempt::build(sweep, selection, resolutions, notes, &configured, oracle)?;
                let onto = match dedupe {
                    Dedupe::Never => None,
                    Dedupe::BuildShape => {
                        let key = attempt.effective_key(sweep, sweep.profile);
                        let onto = shapes.iter().find(|(k, _)| *k == key).map(|(_, j)| *j);
                        if onto.is_none() {
                            shapes.push((key, i));
                        }
                        onto
                    }
                    Dedupe::Execution(debug_of) => {
                        let p = ExecProjection::of(sweep, &attempt, debug_of(sweep));
                        let onto = projections.iter().find(|(k, _)| *k == p).map(|(_, j)| *j);
                        if onto.is_none() {
                            projections.push((p, i));
                        }
                        onto
                    }
                };
                entries.push(match onto {
                    Some(onto) => {
                        let drops = attempt.drop_records();
                        LaneEntry::Deduped(Deduped { onto, selection: attempt.selection, notes: attempt.notes, drops })
                    }
                    None => LaneEntry::Attempt(attempt),
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
    /// The lane's [`EffectiveKey`] - every run request (effective selection
    /// WITHOUT provenance, resolution boundary, projected features) and
    /// support request, unification, rustflags, env - with the effective
    /// debug answer in place of the configured profile (which `--debug`/
    /// `--release` can override), and `doc_only` - a sweep and its doctest
    /// twin run different things. Admission notes are outside it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct ExecProjection {
        key: EffectiveKey,
        debug: bool,
        doc_only: bool,
    }

    impl ExecProjection {
        fn of(sweep: &ResolvedSweep, attempt: &Attempt, debug: bool) -> Self {
            Self { key: attempt.effective_key(sweep, None), debug, doc_only: sweep.doc_only }
        }
    }

    /// One line per dropped token per lane: the run (or support build) that
    /// does not carry it, and the members that route it. Rendered once per
    /// lane however many phases share the request - the same text from two
    /// phases is one line.
    pub(crate) fn feature_drop_lines(sweeps: &[ResolvedSweep], phases: &[&PhaseSelection]) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut push = |line: String| {
            if !lines.contains(&line) {
                lines.push(line);
            }
        };
        for phase in phases.iter().filter(|p| p.is_enabled()) {
            for (sweep, entry) in sweeps.iter().zip(phase.entries()) {
                // Attempted and deduped lanes alike: a dedupe decides who runs,
                // never whether a lane's narrowing is said.
                let drops = match entry {
                    LaneEntry::Attempt(a) => a.drop_records(),
                    LaneEntry::Deduped(d) => d.drops.clone(),
                    _ => continue,
                };
                for (scope, d) in &drops {
                    push(drop_line(&sweep.label, scope, d));
                }
            }
        }
        lines
    }

    impl Attempt {
        /// Every token this lane's requests dropped, with the request's scope
        /// as the announcement names it (`-p x`, `build y`).
        fn drop_records(&self) -> Vec<(String, DroppedToken)> {
            let mut out = Vec::new();
            for run in &self.runs {
                let scope = match run.selection.packages() {
                    Some(p) => p.iter().map(|p| format!("-p {p}")).collect::<Vec<_>>().join(" "),
                    None => "default selection".to_owned(),
                };
                out.extend(run.dropped.iter().map(|d| (scope.clone(), d.clone())));
            }
            for s in &self.support {
                out.extend(s.dropped.iter().map(|d| (format!("build {}", s.package), d.clone())));
            }
            out
        }
    }

    fn drop_line(label: &str, scope: &str, d: &DroppedToken) -> String {
        format!(
            "sweep {label}, {scope}: dropped {} (applies to {} only)",
            d.token,
            d.routed_by.join(", ")
        )
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
            oracle: &FeatureOracle<'_>,
        ) -> Result<Self, DevError> {
            let phase = |p: SelectionPhase, off: Option<&'static str>| match off {
                Some(reason) => Ok(PhaseSelection::disabled(p, sweeps.len(), reason)),
                None => PhaseSelection::for_check(p, sweeps, cli, oracle),
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

        /// `for_check` over sweeps whose features never need metadata.
        fn for_check(phase: SelectionPhase, sweeps: &[ResolvedSweep], cli: &[String]) -> Result<PhaseSelection, DevError> {
            PhaseSelection::for_check(phase, sweeps, cli, &FeatureOracle::unavailable())
        }

        fn for_brokkr_test(
            sweeps: &[ResolvedSweep],
            package: &str,
            source: PackageSource,
            debug_of: &dyn Fn(&ResolvedSweep) -> bool,
        ) -> Result<PhaseSelection, DevError> {
            PhaseSelection::for_brokkr_test(sweeps, package, source, debug_of, &FeatureOracle::unavailable())
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
            let clippy = for_check(SelectionPhase::Clippy, &sweeps, &[]).unwrap();
            assert_eq!(clippy.attempt(0).unwrap().selection(), &Selection::Bare);
            let test = for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
            assert_eq!(test.attempt(0).unwrap().selection(), &Selection::WorkspaceExcluding(s(&["x"])));
            assert_eq!(package_args(test.attempt(0).unwrap().selection()), s(&["--workspace", "--exclude", "x"]));

            // Under `-p x` the exclusion rules the lane out of the test phase
            // and nowhere else.
            let x = s(&["x"]);
            let clippy = for_check(SelectionPhase::Clippy, &sweeps, &x).unwrap();
            assert_eq!(clippy.attempt(0).unwrap().selection(), &cli(&["x"]));
            let test = for_check(SelectionPhase::Test, &sweeps, &x).unwrap();
            assert!(matches!(test.entry(0), Some(LaneEntry::Excluded(_))));
            assert_eq!(
                test.entry(0).unwrap().skip_reason().unwrap(),
                "-p x is in this sweep's test_exclude_packages"
            );
        }

        #[test]
        fn an_override_replaces_the_sweeps_own_selection_with_what_its_rules_kept() {
            let sweeps = vec![listed("ffi", &["a", "b"])];
            let sel = for_check(SelectionPhase::Test, &sweeps, &s(&["a", "x"])).unwrap();
            let attempt = sel.attempt(0).unwrap();
            assert_eq!(attempt.selection(), &cli(&["a"]));
            assert_eq!(attempt.notes().len(), 1);
            assert_eq!(package_args(attempt.selection()), s(&["-p", "a"]));
            // Without an override the sweep's own list stands.
            let own = for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
            assert_eq!(own.attempt(0).unwrap().selection(), &Selection::Explicit(s(&["a", "b"])));
        }

        #[test]
        fn package_mode_over_a_selection_naming_no_packages_is_an_error() {
            let bare = package_mode(sweep("pkg"));
            let err = for_check(SelectionPhase::Clippy, &[bare], &[]).unwrap_err().to_string();
            assert!(err.contains("names no packages"), "{err}");
            let excl = package_mode(excluding("pkg", &["x"]));
            assert!(for_check(SelectionPhase::Test, &[excl], &[]).is_err());
        }

        #[test]
        fn per_package_resolutions_equal_the_selection() {
            let sweeps = vec![package_mode(listed("pkg", &["a", "b", "c"]))];
            let own = for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
            let attempt = own.attempt(0).unwrap();
            assert_eq!(attempt.resolutions(), &Resolutions::PerPackage(s(&["a", "b", "c"])));
            let runs = attempt.runs();
            assert_eq!(runs.len(), 3);
            assert_eq!(runs[1].resolution.as_deref(), Some("b"));
            assert_eq!(runs[1].selection, Selection::Explicit(s(&["b"])));

            // Narrowed by `-p`, in invocation order, each once.
            let narrowed = for_check(SelectionPhase::Test, &sweeps, &s(&["c", "a", "c"])).unwrap();
            let attempt = narrowed.attempt(0).unwrap();
            assert_eq!(attempt.resolutions(), &Resolutions::PerPackage(s(&["c", "a"])));
            assert_eq!(attempt.selection().packages().unwrap(), s(&["c", "a"]).as_slice());
            assert_eq!(attempt.runs()[0].selection, cli(&["c"]));

            // Any other mode is one combined resolution over the selection.
            let combined = for_check(SelectionPhase::Test, &[listed("x", &["a", "b"])], &[]).unwrap();
            let runs = combined.attempt(0).unwrap().runs();
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].resolution, None);
            assert_eq!(runs[0].selection, Selection::Explicit(s(&["a", "b"])));
        }

        #[test]
        fn diagnostics_dedupe_only_onto_an_earlier_attempt() {
            // Two lanes of one entry: one build shape.
            let sweeps = vec![listed("tier1/x", &["a"]), listed("tier2/x", &["a"]), listed("other", &["b"])];
            let sel = for_check(SelectionPhase::Clippy, &sweeps, &[]).unwrap();
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
            let sel = for_check(SelectionPhase::Clippy, &sweeps, &s(&["z"])).unwrap();
            assert!(sel.entries().iter().all(|e| matches!(e, LaneEntry::Excluded(_))));

            // The test phase never dedupes.
            let sweeps = vec![listed("tier1/x", &["a"]), listed("tier2/x", &["a"])];
            let test = for_check(SelectionPhase::Test, &sweeps, &[]).unwrap();
            assert!(test.entries().iter().all(|e| e.attempt().is_some()));
        }

        #[test]
        fn rustdoc_does_not_apply_to_doctest_carriers() {
            let doc = ResolvedSweep { doc_only: true, ..excluding("docs", &["bin"]) };
            let sweeps = vec![sweep("default"), doc];
            let sel = for_check(SelectionPhase::Rustdoc, &sweeps, &s(&["lib"])).unwrap();
            assert!(sel.attempt(0).is_some());
            match sel.entry(1).unwrap() {
                LaneEntry::NotApplicable(n) => assert_eq!(n.admission().kept, s(&["lib"])),
                other => panic!("expected not applicable, got {other:?}"),
            }
            // A rustdoc phase over carriers alone is not refused.
            let only_doc = vec![ResolvedSweep { doc_only: true, ..sweep("docs") }];
            let sel = for_check(SelectionPhase::Rustdoc, &only_doc, &s(&["x"])).unwrap();
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
            let oracle = FeatureOracle::unavailable();
            let all = CheckSelections::build(std::slice::from_ref(&invalid), &[], &skip, false, None, None, &oracle)
                .unwrap();
            assert!(!all.clippy.is_enabled() && !all.test.is_enabled() && !all.rustdoc.is_enabled());
            // The same sweep in an enabled phase is the construction error.
            assert!(CheckSelections::build(&[invalid], &[], &|_| false, false, None, None, &oracle).is_err());
        }

        #[test]
        fn a_refusal_is_stored_not_raised() {
            let sweeps = vec![listed("ffi", &["a"]), listed("vm", &["b"])];
            let test = for_check(SelectionPhase::Test, &sweeps, &s(&["z"])).unwrap();
            assert_eq!(
                test.refusal(),
                Some("-p z: every sweep's config rules the selection out; zero tests ran")
            );
            assert!(test.check_refusal().is_err());
            let clippy = for_check(SelectionPhase::Clippy, &sweeps, &s(&["z"])).unwrap();
            assert_eq!(
                clippy.refusal(),
                Some("-p z: every sweep's config rules the selection out (ffi, vm); nothing reached clippy")
            );
            // One admitting sweep is enough; no `-p` never refuses.
            let ok = for_check(SelectionPhase::Test, &sweeps, &s(&["a"])).unwrap();
            assert!(ok.refusal().is_none());
            assert!(for_check(SelectionPhase::Test, &sweeps, &[]).unwrap().refusal().is_none());
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
                for_brokkr_test(&[dev.clone(), rel.clone()], "a", PackageSource::Cli, &debug_of(true))
                    .unwrap();
            assert!(matches!(sel.entry(1), Some(LaneEntry::Deduped(d)) if d.onto() == 0));
            // Without the override each pin decides its own debug answer.
            let pinned = |s: &ResolvedSweep| s.profile == Some(SweepProfile::Dev);
            let sel = for_brokkr_test(&[dev, rel], "a", PackageSource::Cli, &pinned).unwrap();
            assert!(sel.entries().iter().all(|e| e.attempt().is_some()));
        }

        #[test]
        fn exec_projection_collapses_two_sweeps_narrowed_to_one_package() {
            let sweeps = vec![listed("ab", &["a", "b"]), listed("ac", &["a", "c"])];
            let sel = for_brokkr_test(&sweeps, "a", PackageSource::DefaultPackage, &debug_of(false))
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
                for_brokkr_test(&[sweep("default"), twin], "a", PackageSource::Cli, &debug_of(false))
                    .unwrap();
            assert!(sel.entries().iter().all(|e| e.attempt().is_some()), "the doc twin is its own run");

            let server = ResolvedSweep { build_packages: s(&["server"]), ..sweep("server") };
            let sel =
                for_brokkr_test(&[sweep("plain"), server], "a", PackageSource::Cli, &debug_of(false))
                    .unwrap();
            assert!(sel.entries().iter().all(|e| e.attempt().is_some()));
        }

        #[test]
        fn exec_projection_ignores_provenance() {
            let sw = sweep("x");
            let oracle = FeatureOracle::unavailable();
            let of = |selection: Selection| {
                let a = Attempt::build(&sw, selection, Resolutions::Combined, Vec::new(), &Selection::Bare, &oracle)
                    .unwrap();
                ExecProjection::of(&sw, &a, false)
            };
            let from_cli = of(cli(&["a"]));
            let from_default = of(Selection::Override {
                packages: s(&["a"]),
                provenance: Provenance::ResolvedPackage(PackageSource::ProjectDefault),
            });
            let explicit = of(Selection::Explicit(s(&["a"])));
            assert_eq!(from_cli, from_default);
            assert_eq!(from_cli, explicit);
        }

        // ---- feature projection ------------------------------------------

        use super::super::features::fixture;

        /// A sweep over `packages` with configured feature `tokens`.
        fn featured(label: &str, packages: &[&str], tokens: &[&str]) -> ResolvedSweep {
            ResolvedSweep {
                features: crate::profile::FeatureConfig { tokens: s(tokens), ..Default::default() },
                ..listed(label, packages)
            }
        }

        const PINERS: [&str; 4] = ["piners-vm", "piners-strategy", "piners-runner", "piners-harness"];
        const PINERS_TOKENS: [&str; 4] =
            ["piners-vm/opcode-counts", "piners-strategy/hotpath", "piners-runner/trace", "piners-harness/bench"];

        fn piners_sweep() -> ResolvedSweep {
            featured("piners", &PINERS, &PINERS_TOKENS)
        }

        #[test]
        fn a_full_run_carries_every_token_without_reading_metadata() {
            // The configured selection is the domain: nothing can drop, and
            // the oracle (which would fail) is never consulted.
            let sel = for_check(SelectionPhase::Clippy, &[piners_sweep()], &[]).unwrap();
            let run = &sel.attempt(0).unwrap().runs()[0];
            assert_eq!(run.feature_args(), s(&["--features", &PINERS_TOKENS.join(",")]).as_slice());
            assert!(run.dropped().is_empty());
        }

        #[test]
        fn dash_p_keeps_only_the_tokens_the_narrowed_selection_routes() {
            let oracle = FeatureOracle::fixed(fixture::piners());
            let sweeps = [piners_sweep()];
            // piners-strategy defines `hotpath` and depends on piners-vm, so
            // `piners-vm/opcode-counts` routes too; runner and harness do not.
            let sel = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &s(&["piners-strategy"]), &oracle)
                .unwrap();
            let run = &sel.attempt(0).unwrap().runs()[0];
            assert_eq!(
                run.cargo_args(),
                s(&["-p", "piners-strategy", "--features", "piners-vm/opcode-counts,piners-strategy/hotpath"])
            );
            let dropped: Vec<&str> = run.dropped().iter().map(|d| d.token.as_str()).collect();
            assert_eq!(dropped, ["piners-runner/trace", "piners-harness/bench"]);
            assert_eq!(
                feature_drop_lines(&sweeps, &[&sel]),
                s(&[
                    "sweep piners, -p piners-strategy: dropped piners-runner/trace (applies to piners-runner, piners-harness only)",
                    "sweep piners, -p piners-strategy: dropped piners-harness/bench (applies to piners-harness only)",
                ])
            );

            // piners-harness depends on every other member: all kept.
            let sel = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &s(&["piners-harness"]), &oracle)
                .unwrap();
            let run = &sel.attempt(0).unwrap().runs()[0];
            assert_eq!(run.feature_args(), s(&["--features", &PINERS_TOKENS.join(",")]).as_slice());
            assert!(run.dropped().is_empty());
        }

        #[test]
        fn brokkr_test_projects_onto_its_one_package() {
            let oracle = FeatureOracle::fixed(fixture::piners());
            let sel = PhaseSelection::for_brokkr_test(
                &[piners_sweep()],
                "piners-vm",
                PackageSource::Cli,
                &debug_of(false),
                &oracle,
            )
            .unwrap();
            let run = &sel.attempt(0).unwrap().runs()[0];
            assert_eq!(run.feature_args(), s(&["--features", "piners-vm/opcode-counts"]).as_slice());
            assert_eq!(run.dropped().len(), 3);
        }

        #[test]
        fn package_mode_projects_each_resolution_without_dash_p() {
            let oracle = FeatureOracle::fixed(fixture::piners());
            let sweeps = [package_mode(piners_sweep())];
            let sel = PhaseSelection::for_check(SelectionPhase::Test, &sweeps, &[], &oracle).unwrap();
            let attempt = sel.attempt(0).unwrap();
            let features: Vec<&[String]> = attempt.runs().iter().map(ResolutionRun::feature_args).collect();
            assert_eq!(
                features,
                [
                    s(&["--features", "piners-vm/opcode-counts"]).as_slice(),
                    s(&["--features", "piners-vm/opcode-counts,piners-strategy/hotpath"]).as_slice(),
                    s(&["--features", "piners-vm/opcode-counts,piners-runner/trace"]).as_slice(),
                    s(&["--features", &PINERS_TOKENS.join(",")]).as_slice(),
                ]
            );
            // The lane-level description carries every token some run carries.
            assert_eq!(attempt.described_features(), s(&["--features", &PINERS_TOKENS.join(",")]).as_slice());
        }

        #[test]
        fn a_token_no_domain_member_routes_is_left_for_cargo() {
            let oracle = FeatureOracle::fixed(fixture::piners());
            let sweeps = [featured("typo", &PINERS, &["piners-vm/opcode-count", "nonsense", "a/b/c"])];
            let sel = PhaseSelection::for_check(SelectionPhase::Clippy, &sweeps, &s(&["piners-runner"]), &oracle)
                .unwrap();
            let run = &sel.attempt(0).unwrap().runs()[0];
            // `piners-vm/...` routes through runner's dependency on vm (the
            // feature name is cargo's to check); the rest route nowhere.
            assert_eq!(run.feature_args(), s(&["--features", "piners-vm/opcode-count,nonsense,a/b/c"]).as_slice());
            assert!(run.dropped().is_empty());
        }

        #[test]
        fn support_builds_project_onto_the_support_package() {
            let oracle = FeatureOracle::fixed(fixture::piners());
            // The lane tests vm and strategy; the support build is the runner.
            let sweep = ResolvedSweep {
                build_packages: s(&["piners-runner"]),
                ..featured("bin", &["piners-vm", "piners-strategy"], &[
                    "piners-vm/opcode-counts",
                    "piners-strategy/hotpath",
                    "piners-runner/trace",
                ])
            };
            let sel = PhaseSelection::for_check(SelectionPhase::Test, std::slice::from_ref(&sweep), &[], &oracle)
                .unwrap();
            let attempt = sel.attempt(0).unwrap();
            let support = &attempt.support()[0];
            assert_eq!(support.package(), "piners-runner");
            assert_eq!(support.feature_args(), s(&["--features", "piners-vm/opcode-counts,piners-runner/trace"]).as_slice());
            assert_eq!(support.package_args(), s(&["-p", "piners-runner"]));
            assert_eq!(support.dropped()[0].token, "piners-strategy/hotpath");
            // The lane's own run IS its configured selection, so it is the
            // identity - `piners-runner/trace` included, which no lane package
            // routes: cargo rejects it there exactly as a full run always has.
            // Only support builds widen the domain by `build_packages`.
            let run = &attempt.runs()[0];
            assert!(run.dropped().is_empty(), "the test run's selection is the domain");
            assert!(run.feature_args()[1].contains("piners-runner/trace"));
            let lines = feature_drop_lines(std::slice::from_ref(&sweep), &[&sel]);
            assert_eq!(
                lines,
                s(&["sweep bin, build piners-runner: dropped piners-strategy/hotpath (applies to piners-strategy only)"])
            );
        }

        #[test]
        fn invocation_explicit_features_are_never_projected() {
            let mut adhoc = piners_sweep();
            adhoc.features.invocation_explicit = true;
            // No oracle consulted, every token kept, under `-p` too.
            let sel = for_check(SelectionPhase::Test, &[adhoc], &s(&["piners-vm"])).unwrap();
            let run = &sel.attempt(0).unwrap().runs()[0];
            assert_eq!(run.feature_args(), s(&["--features", &PINERS_TOKENS.join(",")]).as_slice());
        }

        #[test]
        fn diagnostics_dedupe_on_the_effective_key() {
            let oracle = FeatureOracle::fixed(fixture::piners());
            // Two sweeps differing only in a token piners-vm does not route:
            // under `-p piners-vm` they compile the same thing.
            let a = featured("a", &PINERS, &["piners-vm/opcode-counts"]);
            let b = featured("b", &PINERS, &["piners-vm/opcode-counts", "piners-runner/trace"]);
            let sweeps = [a.clone(), b.clone()];
            let narrowed =
                PhaseSelection::for_check(SelectionPhase::Clippy, &sweeps, &s(&["piners-vm"]), &oracle).unwrap();
            assert!(matches!(narrowed.entry(1), Some(LaneEntry::Deduped(d)) if d.onto() == 0));
            // Without `-p` their requests differ: both attempted.
            let full = PhaseSelection::for_check(SelectionPhase::Clippy, &sweeps, &[], &oracle).unwrap();
            assert!(full.entries().iter().all(|e| e.attempt().is_some()));
            // A compile input outside the requests still keeps them apart.
            let c = ResolvedSweep { rustflags: s(&["--cfg", "x"]), ..a };
            let apart =
                PhaseSelection::for_check(SelectionPhase::Clippy, &[b, c], &s(&["piners-vm"]), &oracle).unwrap();
            assert!(apart.entries().iter().all(|e| e.attempt().is_some()));
        }

        #[test]
        fn brokkr_test_dedupes_on_projected_features() {
            let oracle = FeatureOracle::fixed(fixture::piners());
            let a = featured("a", &PINERS, &["piners-vm/opcode-counts"]);
            let b = featured("b", &PINERS, &["piners-vm/opcode-counts", "piners-harness/bench"]);
            let sweeps = [a, b];
            let sel = PhaseSelection::for_brokkr_test(&sweeps, "piners-vm", PackageSource::Cli, &debug_of(false), &oracle)
                .unwrap();
            assert!(matches!(sel.entry(1), Some(LaneEntry::Deduped(d)) if d.onto() == 0));
            // The deduped lane's own drop is still said: folding it onto `a`
            // decides who runs, not whether its narrowing is announced.
            assert_eq!(
                feature_drop_lines(&sweeps, &[&sel]),
                s(&["sweep b, -p piners-vm: dropped piners-harness/bench (applies to piners-harness only)"])
            );
        }

        #[test]
        fn brokkr_test_never_dedupes_an_excluded_lane() {
            let sweeps = vec![excluding("excl", &["a"]), sweep("plain")];
            let sel = for_brokkr_test(&sweeps, "a", PackageSource::Cli, &debug_of(false)).unwrap();
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
    feature_drop_lines, join_notes, AdmissionNote, AdmissionRule, Attempt, CheckSelections,
    InstallSelection, LaneEntry, PackageSource, PhaseSelection, ReachLedger, ResolutionRun, Selection,
    SelectionPhase, SupportRequest,
};
#[cfg(test)]
pub(crate) use selection::package_args;
