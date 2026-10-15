use super::{LintsConfig, ParallelBinaries, SweepProfile};

/// The `-A <lint>` flag pairs the test phase injects into the build's
/// rustflags: `allow` plus the lint names behind `allow_exact`, deduped and
/// order-preserving.
///
/// Only lints rustc itself can raise are kept. Rustflags are part of every
/// unit's fingerprint, so a flag here makes the test phase's artifacts
/// different from what a plain `cargo build`, or rust-analyzer, builds in the
/// same target dir - each then rebuilds the whole graph over the other's. A
/// flag that cannot change a diagnostic is pure cost, so these are left out:
///
/// - `clippy::` and `rustdoc::` names: rustc accepts tool-qualified flags for
///   tools it knows, but raises none of their lints in a build. (A lint shared
///   by clippy and rustc is written unqualified.)
/// - cargo's own lints ([`is_cargo_lint`]): rustc knows no `cargo` lint tool,
///   so `-A cargo::...` would itself raise a diagnostic.
///
/// A free function taking the complete config so callers cannot accidentally
/// separate the blanket and sited entries while deciding what reaches a build.
pub fn test_phase_allow_flags(lints: &LintsConfig) -> Vec<String> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::new();
    let names = lints
        .allow
        .iter()
        .chain(lints.allow_exact.iter().map(|s| &s.lint))
        .filter(|name| name.reaches_test_build());
    for name in names {
        if seen.insert(name.as_str()) {
            out.push("-A".to_string());
            out.push(name.as_str().to_string());
        }
    }
    out
}

/// The tool that owns a lint's namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LintTool {
    Rustc,
    Clippy,
    Rustdoc,
    Cargo,
}

/// An owned lint identity. Config stores this parsed form so namespace checks
/// do not repeat for each phase or diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LintName {
    tool: LintTool,
    name: String,
    spelling: String,
}

impl LintName {
    pub fn parse(text: &str) -> Self {
        for (prefix, tool) in [
            ("clippy::", LintTool::Clippy),
            ("rustdoc::", LintTool::Rustdoc),
            ("cargo::", LintTool::Cargo),
        ] {
            if let Some(name) = text.strip_prefix(prefix) {
                return Self {
                    tool,
                    name: name.to_owned(),
                    spelling: text.to_owned(),
                };
            }
        }
        Self {
            tool: LintTool::Rustc,
            name: text.to_owned(),
            spelling: text.to_owned(),
        }
    }

    /// The exact spelling supplied by config or reported by cargo.
    pub fn as_str(&self) -> &str {
        &self.spelling
    }

    pub fn tool(&self) -> LintTool {
        self.tool
    }

    pub fn reaches_test_build(&self) -> bool {
        self.tool == LintTool::Rustc
    }

    /// Clippy accepts an unqualified spelling too. Rustdoc and cargo names
    /// must retain their namespaces; lint groups do not match member codes.
    pub fn matches(&self, code: &Self) -> bool {
        self.name == code.name
            && (self.tool == code.tool
                || matches!(
                    (self.tool, code.tool),
                    (LintTool::Rustc, LintTool::Clippy) | (LintTool::Clippy, LintTool::Rustc)
                ))
    }
}

/// Whether an allow becomes a test-phase build flag: only a lint
/// rustc itself raises does ([`test_phase_allow_flags`]).
pub fn reaches_test_build(name: &LintName) -> bool {
    name.reaches_test_build()
}

/// Whether a lint name belongs to cargo's own namespace (`[lints.cargo]`:
/// `cargo::unused_dependencies`, ...). Cargo reports these itself, on stderr,
/// so an allow for one acts only where brokkr ingests diagnostics, never as a
/// rustc `-A` flag.
pub fn is_cargo_lint(name: &LintName) -> bool {
    name.tool() == LintTool::Cargo
}

/// A cargo feature-unification mode, with brokkr's `auto` policy resolved
/// away. This is what actually reaches an argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CargoUnification {
    Selected,
    Workspace,
    Package,
}

impl CargoUnification {
    /// The value as cargo spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::Workspace => "workspace",
            Self::Package => "package",
        }
    }

    /// The argv fragment that pins this mode. Both halves are required:
    /// the config key is unstable, so the `-Z` must accompany it unless the
    /// consuming repo carries `[unstable] feature-unification = true` - and
    /// relying on that is the same class of ambient accident this mode
    /// exists to catch.
    pub fn cargo_args(self) -> [String; 3] {
        [
            "-Zfeature-unification".into(),
            "--config".into(),
            format!("resolver.feature-unification=\"{}\"", self.as_str()),
        ]
    }
}

/// The resolved unification policy for one sweep in one invocation.
///
/// `Ambient` is not a cargo mode - it is the absence of a pin, which leaves
/// whatever the config chain says in force. Keeping it distinct from
/// `Pinned(Selected)` is what lets `auto` preserve existing behaviour byte
/// for byte while explicit `selected` makes a real promise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffectiveUnification {
    /// No pin. The default, and what `auto` resolves to outside the parallel
    /// lane's promotion - i.e. what every sweep did before this key existed.
    #[default]
    Ambient,
    Pinned(CargoUnification),
}

impl EffectiveUnification {
    /// The argv fragment, empty when nothing is pinned.
    pub fn cargo_args(self) -> Vec<String> {
        match self {
            Self::Ambient => Vec::new(),
            Self::Pinned(mode) => mode.cargo_args().to_vec(),
        }
    }

    /// Whether this mode requires one cargo resolution per package.
    pub fn is_per_package(self) -> bool {
        self == Self::Pinned(CargoUnification::Package)
    }

    /// How the mode reads in a run line. `Ambient` prints nothing - it is
    /// the status quo and naming it on every line would be noise.
    pub fn describe(self) -> Option<&'static str> {
        match self {
            Self::Ambient => None,
            Self::Pinned(mode) => Some(mode.as_str()),
        }
    }
}

impl SweepProfile {
    /// The subdirectory of `target/` this profile's artifacts land in -
    /// what `BROKKR_TEST_BIN_DIR` must point at.
    pub fn target_subdir(self) -> &'static str {
        match self {
            Self::Dev => "debug",
            Self::Release => "release",
        }
    }

    /// The cargo argv fragment that selects this profile. Dev is cargo's
    /// default and needs no flag.
    #[cfg(test)]
    pub fn cargo_args(self) -> Vec<String> {
        crate::build_purpose::profile_args(
            crate::build_purpose::Purpose::Check,
            &crate::build_purpose::Profile::from(self),
        )
    }
}

/// Stable short hex digest (FNV-1a, 64-bit) of a rustflags token list, used to
/// name a sweep's isolated target dir. Deterministic across runs and machines
/// so the isolated `target/rustflags-<key>` cache persists between invocations
/// and is shared by every sweep carrying identical flags. Returns `None` for an
/// empty flag list (no isolation).
///
/// KNOWN GAP, deliberately left: `join(" ")` is ambiguous at the token level -
/// `["--cfg", "a b"]` and `["--cfg a", "b"]` hash alike. Fixing it means a
/// length-tagged encoding, which renames every existing isolated dir and costs
/// one cold rebuild per rustflags sweep in every consuming repo. The collision
/// needs a flag list carrying an embedded space to bite, so the rename is not
/// worth forcing on a config nobody has written; it is recorded here rather
/// than fixed silently. Any change here must keep the digest stable for a given
/// flag list or the dirs churn.
pub fn rustflags_target_key(rustflags: &[String]) -> Option<String> {
    if rustflags.is_empty() {
        return None;
    }
    let joined = rustflags.join(" ");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in joined.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(format!("{hash:016x}"))
}

impl ParallelBinaries {
    /// The budget to run under, resolving the unset case against this
    /// machine's cache domain.
    ///
    /// Resolved at config load rather than at run time so the number is
    /// visible to everything downstream - `brokkr env` reports the same
    /// figure this returns, which is what makes a machine-dependent default
    /// auditable rather than mysterious.
    pub fn resolved_budget(self) -> u32 {
        self.budget
            .unwrap_or_else(crate::cpu_topology::cache_domain_cores_or_default)
    }
}
