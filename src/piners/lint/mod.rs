//! `brokkr lint-corpus`: the piners differential-lint corpus.
//!
//! Runs a keyword-selected slice of `.pine` snippets through two offline
//! validators - **piners** (this dirty tree, brokkr-compiled) and
//! **pine-lint** (pre-installed) - diffs their diagnostics on a
//! `(line, col, severity)` grain, and gates on a pinned agreement
//! disposition per snippet. A periodic `--reanchor` mode consults
//! TradingView (`pine-lint --tv`) to re-ground the corpus.
//!
//! The trade-parity sibling is `brokkr corpus` (`src/piners/`); this
//! mirrors its structure minus the OHLCV feeds, trade oracle and manifest
//! harness. See `docs/commands/lint-corpus.md`.

pub mod cmd;
pub mod db;
pub mod diff;
pub mod lints_write;
pub mod query;
pub mod registry;
pub mod reseed;
pub mod select;
pub mod validators;

use std::collections::BTreeSet;

use serde::Deserialize;

/// The dispositions a lint pin may accept. Validator failures are run
/// failures, not states of a snippet, so they are intentionally absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinnableDisposition {
    AgreeClean,
    AgreeFlagged,
    Divergent,
}

impl PinnableDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgreeClean => "agree_clean",
            Self::AgreeFlagged => "agree_flagged",
            Self::Divergent => "divergent",
        }
    }
}

/// The two gated diagnostic severities. piners also emits `hint`, but
/// pine-lint has no counterpart, so hints are dropped before the diff
/// (informational only - never a `piners_only` divergence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One normalized diagnostic, reduced to the gated diff grain: position
/// plus severity. Message text is deliberately excluded - wording can drift
/// in either tool without changing the disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DiagKey {
    pub line: usize,
    /// Column as the tool reported it (1-based). `None` when a tool omits it.
    pub col: Option<usize>,
    pub severity: Severity,
}

/// A validator's normalized output for one snippet: the deduplicated set of
/// diagnostic keys (error+warning only). An ordered set so the diff and the
/// rendering are deterministic.
pub type DiagSet = BTreeSet<DiagKey>;

/// One probe's fully-resolved result for a run: its typed disposition, the
/// gate verdict against its pin, raw diagnostic counts, and TV anchor
/// relationship. The single unit `cmd` builds, the run store
/// ([`db`]) ingests, and `lint-results` renders.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub probe: String,
    /// Classified validator result. The variant carries a divergence
    /// signature or identifies which validator failed.
    pub disposition: diff::LintDisposition,
    /// The pinned `expected` at run time; `None` = never blessed.
    pub expected: Option<PinnableDisposition>,
    /// Whether the actual disposition satisfied the pin.
    pub gate_ok: bool,
    /// Count of piners diagnostics (error+warning).
    pub piners_count: usize,
    /// Count of pine-lint diagnostics (error+warning).
    pub lint_count: usize,
    /// Failure reason for a `piners_error`/`lint_error` disposition.
    pub error: Option<String>,
    /// When the probe's TV anchor was last refreshed; `None` = never anchored.
    pub tv_anchored_at: Option<String>,
    /// Whether piners' output diverges from a present TV anchor (informational;
    /// `None` when the probe has no anchor). The shared-but-wrong signal.
    pub tv_divergent: Option<bool>,
}
